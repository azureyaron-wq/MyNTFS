//! Mounted NTFS volume: safety gates, engine (am-fs-ntfs), streaming copy.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use fs_ntfs::attr_io::AttrType;
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::{self, Filesystem};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::{fsck, read, write};
use ntfs_core::{BlockDevice, Error, Result, SafetyReport, VolumeProbe, WritePolicy};
use ntfs_io::FileDevice;

pub mod device_gate;
pub use device_gate::{
    is_block_device, refuse_block_device_format, refuse_block_device_write, DeviceWriteConfirm,
};

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub record: u64,
}

#[derive(Debug, Clone)]
pub struct Stat {
    pub size: u64,
    pub is_dir: bool,
    pub record: u64,
    pub mtime_sec: i64,
}

enum Engine {
    /// Disk images: path facade (may reopen the image file).
    Path(Filesystem),
    /// USB / authopen: owned FD, never reopened by pathname.
    Fd(Mutex<FdIo>),
}

struct FdIo {
    device: FileDevice,
    size: u64,
}

impl BlockIo for FdIo {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> std::result::Result<(), String> {
        BlockDevice::read_at(&self.device, offset, buf).map_err(|e| e.to_string())
    }

    fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> std::result::Result<(), String> {
        if !self.device.writable() {
            return Err("file descriptor is not writable".into());
        }
        BlockDevice::write_at(&self.device, offset, buf).map_err(|e| e.to_string())
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn sync(&mut self) -> std::result::Result<(), String> {
        self.device.flush().map_err(|e| e.to_string())
    }
}

/// A volume the caller has exclusive access to.
pub struct Volume {
    path: PathBuf,
    engine: Engine,
    writable: bool,
    safety: SafetyReport,
    serial: Option<u64>,
}

impl Volume {
    pub fn mount(path: impl AsRef<Path>, policy: WritePolicy) -> Result<Self> {
        Self::mount_with_confirm(path, policy, DeviceWriteConfirm::default())
    }

    pub fn mount_with_confirm(
        path: impl AsRef<Path>,
        policy: WritePolicy,
        confirm: DeviceWriteConfirm,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let probe_dev = FileDevice::open_ro(&path)?;
        let safety = VolumeProbe::probe(&probe_dev)?;
        drop(probe_dev);
        let want_write = matches!(policy, WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite);
        if want_write {
            refuse_block_device_write(&path, confirm)?;
            if !safety.writable(policy) {
                return Err(Error::WriteDenied("safety gates refused write"));
            }
        }
        let writable = want_write && safety.writable(policy);
        if writable {
            backup_metadata(&path)?;
        }
        let fs = if writable {
            Filesystem::mount_rw(&path).map_err(|e| Error::Corrupt(e.to_string()))?
        } else {
            Filesystem::mount(&path).map_err(|e| Error::Corrupt(e.to_string()))?
        };
        let serial = fs.volume_info().ok().map(|i| i.serial_number);
        Ok(Self {
            path,
            engine: Engine::Path(fs),
            writable,
            safety,
            serial,
        })
    }

    /// Mount from an already-open file. Never reopens `display_path`.
    pub fn mount_from_file(
        file: File,
        display_path: impl AsRef<Path>,
        policy: WritePolicy,
        confirm: DeviceWriteConfirm,
    ) -> Result<Self> {
        let path = display_path.as_ref().to_path_buf();
        let want_write = matches!(policy, WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite);
        let probe = FileDevice::from_file(file.try_clone()?, false, &path)?;
        let safety = VolumeProbe::probe(&probe)?;
        drop(probe);
        if want_write {
            refuse_block_device_write(&path, confirm)?;
            if !safety.writable(policy) {
                return Err(Error::WriteDenied("safety gates refused write"));
            }
        }
        let writable = want_write && safety.writable(policy);
        let device = FileDevice::from_file(file, writable, &path)?;
        let size = device.size();
        if size == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "could not determine device size from file descriptor",
            )));
        }
        if writable {
            backup_from_file(device.inner(), &path)?;
        }
        let mut io = FdIo { device, size };
        if writable {
            let _ = fsck::upgrade_volume_version_io(&mut io);
        }
        let serial = read::read_volume_info(&mut io)
            .ok()
            .map(|info| info.serial_number);
        Ok(Self {
            path,
            engine: Engine::Fd(Mutex::new(io)),
            writable,
            safety,
            serial,
        })
    }

    /// Duplicate `fd` and mount. The caller's descriptor stays open.
    #[cfg(unix)]
    pub fn mount_from_raw_fd(
        fd: i32,
        display_path: impl AsRef<Path>,
        policy: WritePolicy,
        confirm: DeviceWriteConfirm,
    ) -> Result<Self> {
        let want_write = matches!(policy, WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite);
        let file = ntfs_io::dup_owned_file(fd, want_write)?;
        Self::mount_from_file(file, display_path, policy, confirm)
    }

    /// Mount from an open file with forced logical-block alignment (test helper).
    #[doc(hidden)]
    pub fn mount_from_file_with_logical_block_size(
        file: File,
        display_path: impl AsRef<Path>,
        policy: WritePolicy,
        confirm: DeviceWriteConfirm,
        logical_block_size: u32,
    ) -> Result<Self> {
        let path = display_path.as_ref().to_path_buf();
        let want_write = matches!(policy, WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite);
        let probe = FileDevice::from_file_with_logical_block_size(
            file.try_clone()?,
            false,
            &path,
            logical_block_size,
        )?;
        let safety = VolumeProbe::probe(&probe)?;
        drop(probe);
        if want_write {
            refuse_block_device_write(&path, confirm)?;
            if !safety.writable(policy) {
                return Err(Error::WriteDenied("safety gates refused write"));
            }
        }
        let writable = want_write && safety.writable(policy);
        let device = FileDevice::from_file_with_logical_block_size(
            file,
            writable,
            &path,
            logical_block_size,
        )?;
        let size = device.size();
        if size == 0 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "could not determine device size from file descriptor",
            )));
        }
        if writable {
            backup_from_file(device.inner(), &path)?;
        }
        let mut io = FdIo { device, size };
        if writable {
            let _ = fsck::upgrade_volume_version_io(&mut io);
        }
        let serial = read::read_volume_info(&mut io)
            .ok()
            .map(|info| info.serial_number);
        Ok(Self {
            path,
            engine: Engine::Fd(Mutex::new(io)),
            writable,
            safety,
            serial,
        })
    }

    /// Safety probe without mounting the write engine.
    pub fn probe_path(path: impl AsRef<Path>) -> Result<SafetyReport> {
        let dev = FileDevice::open_ro(path)?;
        VolumeProbe::probe(&dev)
    }

    pub fn probe_file(file: &File) -> Result<SafetyReport> {
        let cloned = file.try_clone()?;
        let dev = FileDevice::from_file(cloned, false, "<fd>")?;
        VolumeProbe::probe(&dev)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    pub fn safety(&self) -> &SafetyReport {
        &self.safety
    }

    pub fn serial(&self) -> Option<u64> {
        self.serial
    }

    pub fn list(&self, dir: &str) -> Result<Vec<DirEntry>> {
        self.dispatch(
            |fs| {
                let ents = fs.read_dir(dir).map_err(|e| Error::Corrupt(e.to_string()))?;
                let mut out = Vec::new();
                for e in ents {
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    let child = join_ntfs(dir, &e.name);
                    let st = fs.stat(&child).ok();
                    out.push(DirEntry {
                        name: e.name,
                        is_dir: matches!(e.file_type, facade::FileType::Directory),
                        size: st.as_ref().map(|s| s.size).unwrap_or(0),
                        record: e.file_record_number,
                    });
                }
                Ok(out)
            },
            |io| {
                let rec = read::resolve_path(io, dir).map_err(|e| Error::NotFound(e))?;
                let ents = read::read_dir_entries(io, rec).map_err(|e| Error::Corrupt(e))?;
                let mut out = Vec::new();
                for e in ents {
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    let child = join_ntfs(dir, &e.name);
                    let child_rec = read::resolve_path(io, &child).ok();
                    let size = child_rec
                        .and_then(|r| read::read_stat(io, r).ok())
                        .map(|s| s.size)
                        .unwrap_or(0);
                    out.push(DirEntry {
                        name: e.name,
                        is_dir: e.is_dir,
                        size,
                        record: e.record_number,
                    });
                }
                Ok(out)
            },
        )
    }

    pub fn stat(&self, path: &str) -> Result<Stat> {
        self.dispatch(
            |fs| {
                let a = fs.stat(path).map_err(|e| Error::NotFound(e.to_string()))?;
                Ok(Stat {
                    size: a.size,
                    is_dir: matches!(a.file_type, facade::FileType::Directory),
                    record: a.file_record_number,
                    mtime_sec: a.mtime_sec,
                })
            },
            |io| {
                let rec = read::resolve_path(io, path).map_err(|e| Error::NotFound(e))?;
                let a = read::read_stat(io, rec).map_err(|e| Error::Corrupt(e))?;
                Ok(Stat {
                    size: a.size,
                    is_dir: a.is_dir,
                    record: rec,
                    mtime_sec: nt_to_unix_sec(a.modified_nt),
                })
            },
        )
    }

    pub fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
        match &self.engine {
            Engine::Path(fs) => fs
                .read_file(path, offset, buf)
                .map_err(|e| Error::Corrupt(e.to_string())),
            Engine::Fd(lock) => {
                let mut io = lock
                    .lock()
                    .map_err(|_| Error::Corrupt("fd io lock poisoned".into()))?;
                let rec = read::resolve_path(&mut *io, path).map_err(Error::NotFound)?;
                let data =
                    read::read_attribute_range(&mut *io, rec, AttrType::Data, None, offset, buf.len())
                        .map_err(Error::Corrupt)?;
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
        }
    }

    pub fn create_file(&self, parent: &str, name: &str) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.create_file(parent, name).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::create_file_io(io, parent, name).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.mkdir(parent, name).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::mkdir_io(io, parent, name).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn unlink(&self, path: &str) -> Result<()> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.unlink(path).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::unlink_io(io, path).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn rmdir(&self, path: &str) -> Result<()> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.rmdir(path).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::rmdir_io(io, path).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn rename(&self, old: &str, new_basename: &str) -> Result<()> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.rename(old, new_basename).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::rename_io(io, old, new_basename).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn write_contents(&self, path: &str, data: &[u8]) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| {
                if fs.stat(path).is_err() {
                    let (parent, name) = split_parent(path)?;
                    fs.create_file(&parent, &name)
                        .map_err(|e| Error::Corrupt(e.to_string()))?;
                }
                fs.write_file_contents(path, data)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| {
                if read::resolve_path(io, path).is_err() {
                    let (parent, name) = split_parent(path)?;
                    write::create_file_io(io, &parent, &name).map_err(|e| Error::Corrupt(e))?;
                }
                write::write_file_contents_io(io, path, data).map_err(|e| Error::Corrupt(e))
            },
        )
    }

    pub fn truncate(&self, path: &str, size: u64) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.truncate(path, size).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::truncate_io(io, path, size).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn grow(&self, path: &str, size: u64) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.grow(path, size).map_err(|e| Error::Corrupt(e.to_string())),
            |io| write::grow_nonresident_io(io, path, size).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn write_named_stream(&self, path: &str, stream: &str, data: &[u8]) -> Result<()> {
        self.require_write()?;
        self.dispatch(
            |fs| {
                fs.write_named_stream(path, stream, data)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| {
                write::write_named_stream_io(io, path, stream, data).map_err(|e| Error::Corrupt(e))
            },
        )
    }

    pub fn clear_dirty(&self) -> Result<bool> {
        self.require_write()?;
        self.dispatch(
            |fs| fs.clear_dirty().map_err(|e| Error::Corrupt(e.to_string())),
            |io| fsck::clear_dirty_io(io).map_err(|e| Error::Corrupt(e)),
        )
    }

    /// Flush media caches before umount / Finder remount.
    /// Path images: `sync_data` on the file. USB FdIo: DKIOC / fsync via FileDevice.
    pub fn sync(&self) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        match &self.engine {
            Engine::Path(_) => {
                let f = std::fs::File::options().write(true).open(&self.path)?;
                f.sync_data()?;
                Ok(())
            }
            Engine::Fd(lock) => {
                let mut io = lock
                    .lock()
                    .map_err(|_| Error::Corrupt("fd io lock poisoned".into()))?;
                io.sync().map_err(Error::Corrupt)
            }
        }
    }

    fn require_write(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::WriteDenied("volume mounted read-only"))
        }
    }

    fn dispatch<T>(
        &self,
        path_f: impl FnOnce(&Filesystem) -> Result<T>,
        fd_f: impl FnOnce(&mut FdIo) -> Result<T>,
    ) -> Result<T> {
        match &self.engine {
            Engine::Path(fs) => path_f(fs),
            Engine::Fd(lock) => {
                let mut io = lock
                    .lock()
                    .map_err(|_| Error::Corrupt("fd io lock poisoned".into()))?;
                fd_f(&mut io)
            }
        }
    }
}

fn nt_to_unix_sec(nt: u64) -> i64 {
    read::nt_to_unix(nt)
}

fn backup_from_file(file: &File, path: &Path) -> Result<PathBuf> {
    let cloned = file.try_clone()?;
    let f = FileDevice::from_file(cloned, false, path)?;
    let mut boot = vec![0u8; 16 * 1024];
    let n = if f.size() == 0 {
        boot.len()
    } else {
        boot.len().min(f.size() as usize)
    };
    f.read_at(0, &mut boot[..n])?;
    boot.truncate(n);
    write_boot_backup(path, &boot)
}

fn split_parent(path: &str) -> Result<(String, String)> {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        return Err(Error::Unsupported("cannot create root"));
    }
    let path = path.trim_start_matches('/');
    let (parent, name) = path
        .rsplit_once('/')
        .map(|(p, n)| (format!("/{p}"), n.to_string()))
        .unwrap_or_else(|| ("/".to_string(), path.to_string()));
    if name.is_empty() {
        return Err(Error::Unsupported("empty filename"));
    }
    Ok((parent, name))
}

fn join_ntfs(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

/// Copy `ntfs_path` out of the volume into `dest` using a bounded buffer ring.
pub fn copy_out(vol: &Volume, ntfs_path: &str, dest: &Path, block: usize) -> Result<CopyStats> {
    let st = vol.stat(ntfs_path)?;
    if st.is_dir {
        return Err(Error::Unsupported("copy_out of directory"));
    }
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)?;
    let block = block.clamp(64 * 1024, 8 * 1024 * 1024);
    let mut buf = vec![0u8; block];
    let mut offset = 0u64;
    let mut bytes = 0u64;
    let t0 = Instant::now();
    loop {
        let n = vol.read(ntfs_path, offset, &mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        offset += n as u64;
        bytes += n as u64;
        if offset >= st.size {
            break;
        }
    }
    out.flush()?;
    Ok(CopyStats {
        bytes,
        elapsed: t0.elapsed(),
    })
}

/// Copy a host file into the NTFS volume (create + write_contents).
pub fn copy_in(vol: &Volume, src: &Path, parent: &str, name: &str) -> Result<CopyStats> {
    vol.require_write()?;
    let mut f = File::open(src)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    let t0 = Instant::now();
    vol.create_file(parent, name)?;
    let dest = join_ntfs(parent, name);
    let n = vol.write_contents(&dest, &data)?;
    Ok(CopyStats {
        bytes: n,
        elapsed: t0.elapsed(),
    })
}

#[derive(Debug, Clone)]
pub struct CopyStats {
    pub bytes: u64,
    pub elapsed: std::time::Duration,
}

impl CopyStats {
    pub fn mbps(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s == 0.0 {
            0.0
        } else {
            (self.bytes as f64 / 1_000_000.0) / s
        }
    }
}

/// Format a blank image as NTFS (refuses block devices).
pub fn format_image(path: &Path, size: u64, label: Option<&str>) -> Result<()> {
    refuse_block_device_format(path)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let _ = FileDevice::create(path, size)?;
    let mut io = PathIo::open_rw(path).map_err(|e| Error::Corrupt(e))?;
    // MFT record size must be 4096 for system records ($Root etc.); 1024 is too small.
    format_filesystem(&mut io, size, 4096, 4096, label, None)
        .map_err(|e| Error::Corrupt(e))?;
    Ok(())
}

fn backup_metadata(path: &Path) -> Result<PathBuf> {
    let mut boot = vec![0u8; 16 * 1024];
    {
        let f = FileDevice::open_ro(path)?;
        let n = boot.len().min(f.size() as usize);
        f.read_at(0, &mut boot[..n])?;
        boot.truncate(n);
    }
    write_boot_backup(path, &boot)
}

fn write_boot_backup(path: &Path, boot: &[u8]) -> Result<PathBuf> {
    let dir = backup_dir();
    fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("volume");
    let dest = dir.join(format!("{name}.boot.bak"));
    fs::write(&dest, boot)?;
    Ok(dest)
}

pub fn backup_dir() -> PathBuf {
    dirs_backup_home().join(".myntfs").join("backup")
}

fn dirs_backup_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn fsck(path: &Path, confirm: DeviceWriteConfirm) -> Result<()> {
    refuse_block_device_write(path, confirm)?;
    fs_ntfs::fsck::fsck(path)
        .map(|_| ())
        .map_err(Error::Corrupt)
}

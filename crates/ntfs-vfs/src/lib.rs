//! Mounted NTFS volume: safety gates, engine (am-fs-ntfs), streaming copy.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use fs_ntfs::attr_io::{self, AttrType};
use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::{self, Filesystem};
use fs_ntfs::mkfs::format_filesystem;
use fs_ntfs::mft_io::update_mft_record_io;
use fs_ntfs::{fsck, read, write};
use ntfs_core::{BlockDevice, Error, Result, SafetyReport, VolumeProbe, WritePolicy};
use ntfs_io::FileDevice;

pub mod device_gate;
mod attr_grow;
mod index_grow;
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

/// Backstop flush. Callers that care about the error string should still
/// invoke `commit_for_unmount` (or FFI `myntfs_sync`) explicitly first.
impl Drop for Volume {
    fn drop(&mut self) {
        if self.writable {
            let _ = self.commit_for_unmount();
        }
    }
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
        let want_write = matches!(
            policy,
            WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite
        );
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
        let want_write = matches!(
            policy,
            WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite
        );
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
        let want_write = matches!(
            policy,
            WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite
        );
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
        let want_write = matches!(
            policy,
            WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite
        );
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
                let ents = fs
                    .read_dir(dir)
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                let mut out = Vec::new();
                for e in ents {
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    let is_dir = matches!(e.file_type, facade::FileType::Directory);
                    let size = if is_dir {
                        0
                    } else {
                        let child = join_ntfs(dir, &e.name);
                        fs.stat(&child).map(|s| s.size).unwrap_or(0)
                    };
                    out.push(DirEntry {
                        name: e.name,
                        is_dir,
                        size,
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
                    let size = if e.is_dir {
                        0
                    } else {
                        read::read_stat(io, e.record_number)
                            .ok()
                            .map(|s| s.size)
                            .unwrap_or(0)
                    };
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
                let data = read::read_attribute_range(
                    &mut *io,
                    rec,
                    AttrType::Data,
                    None,
                    offset,
                    buf.len(),
                )
                .map_err(Error::Corrupt)?;
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
        }
    }

    pub fn create_file(&self, parent: &str, name: &str) -> Result<u64> {
        self.require_write()?;
        self.with_index_grow(parent, || self.create_file_once(parent, name))
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<u64> {
        self.require_write()?;
        self.with_index_grow(parent, || self.mkdir_once(parent, name))
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

    /// Delete a file, or a folder and everything inside it.
    pub fn remove(&self, path: &str) -> Result<()> {
        self.require_write()?;
        let mut files = 0u32;
        self.remove_limited(path, 0, &mut files)
    }

    pub fn rename(&self, old: &str, new_basename: &str) -> Result<()> {
        self.require_write()?;
        self.dispatch(
            |fs| {
                fs.rename(old, new_basename)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| write::rename_io(io, old, new_basename).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn write_contents(&self, path: &str, data: &[u8]) -> Result<u64> {
        self.require_write()?;
        if self.stat(path).is_err() {
            let (parent, name) = split_parent(path)?;
            self.create_file(&parent, &name)?;
        }
        self.dispatch(
            |fs| {
                fs.write_file_contents(path, data)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| write::write_file_contents_io(io, path, data).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<u64> {
        self.require_write()?;
        let n = self.dispatch(
            |fs| {
                write::write_at(fs.image_path(), path, offset, data)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| write::write_at_io(io, path, offset, data).map_err(|e| Error::Corrupt(e)),
        )?;
        let end = offset.saturating_add(n);
        self.dispatch(
            |fs| {
                let mut io = PathIo::open_rw(fs.image_path()).map_err(Error::Corrupt)?;
                bump_data_initialized(&mut io, path, end)
            },
            |io| bump_data_initialized(io, path, end),
        )?;
        Ok(n)
    }

    /// `(total_bytes, free_bytes)`. Free space walks `$Bitmap` once.
    pub fn volume_space(&self) -> Result<(u64, u64)> {
        self.dispatch(
            |fs| {
                let info = fs
                    .volume_info()
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                let stats = fs
                    .volume_stats()
                    .map_err(|e| Error::Corrupt(e.to_string()))?;
                Ok((
                    info.total_size,
                    stats.free_clusters.saturating_mul(u64::from(info.cluster_size)),
                ))
            },
            |io| {
                let info = read::read_volume_info(io).map_err(Error::Corrupt)?;
                let bm = fs_ntfs::bitmap::locate_bitmap_io(io).map_err(Error::Corrupt)?;
                let free = fs_ntfs::bitmap::count_free_io(io, &bm).map_err(Error::Corrupt)?;
                Ok((
                    info.total_size,
                    free.saturating_mul(u64::from(info.cluster_size)),
                ))
            },
        )
    }

    pub fn truncate(&self, path: &str, size: u64) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| {
                fs.truncate(path, size)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| write::truncate_io(io, path, size).map_err(|e| Error::Corrupt(e)),
        )
    }

    pub fn grow(&self, path: &str, size: u64) -> Result<u64> {
        self.require_write()?;
        self.dispatch(
            |fs| {
                let mut io = PathIo::open_rw(fs.image_path()).map_err(Error::Corrupt)?;
                attr_grow::grow_io(&mut io, path, size)
            },
            |io| attr_grow::grow_io(io, path, size),
        )
    }

    /// Allocate clusters for `path` up to `size` bytes (initialized_length stays 0
    /// until `write_at`). Resizes `$DATA` when mapping-pairs need more room.
    pub fn preallocate(&self, path: &str, size: u64) -> Result<()> {
        self.require_write()?;
        self.dispatch(
            |fs| {
                let mut io = PathIo::open_rw(fs.image_path()).map_err(Error::Corrupt)?;
                attr_grow::preallocate_io(&mut io, path, size)
            },
            |io| attr_grow::preallocate_io(io, path, size),
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

    /// After a successful RW session, reset `$LogFile` to 0xFF and clear
    /// the dirty bit so Windows mounts the volume read-write and does not
    /// replay a journal that never recorded our writes. Then flush media.
    pub fn commit_for_unmount(&self) -> Result<()> {
        if !self.writable {
            return Ok(());
        }
        match &self.engine {
            Engine::Path(_) => {
                let mut io = PathIo::open_rw(&self.path).map_err(Error::Corrupt)?;
                fsck::fsck_io(&mut io, None).map_err(Error::Corrupt)?;
            }
            Engine::Fd(lock) => {
                let mut io = lock
                    .lock()
                    .map_err(|_| Error::Corrupt("fd io lock poisoned".into()))?;
                fsck::fsck_io(&mut *io, None).map_err(Error::Corrupt)?;
            }
        }
        self.sync()
    }

    fn require_write(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::WriteDenied("volume mounted read-only"))
        }
    }

    fn create_file_once(&self, parent: &str, name: &str) -> Result<u64> {
        self.dispatch(
            |fs| {
                fs.create_file(parent, name)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| write::create_file_io(io, parent, name).map_err(|e| Error::Corrupt(e)),
        )
    }

    fn mkdir_once(&self, parent: &str, name: &str) -> Result<u64> {
        self.dispatch(
            |fs| {
                fs.mkdir(parent, name)
                    .map_err(|e| Error::Corrupt(e.to_string()))
            },
            |io| write::mkdir_io(io, parent, name).map_err(|e| Error::Corrupt(e)),
        )
    }

    fn with_index_grow<T>(&self, parent: &str, op: impl Fn() -> Result<T>) -> Result<T> {
        match op() {
            Ok(v) => Ok(v),
            Err(e) if index_grow::is_index_capacity_error(&e) => {
                match self.promote_parent_index(parent) {
                    Ok(true) => op().map_err(index_grow::map_index_error),
                    Ok(false) => Err(index_grow::map_index_error(e)),
                    Err(grow) => {
                        if index_grow::is_index_capacity_error(&grow) {
                            Err(index_grow::map_index_error(e))
                        } else {
                            Err(grow)
                        }
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    fn promote_parent_index(&self, parent: &str) -> Result<bool> {
        match &self.engine {
            Engine::Path(_) => {
                let mut io = PathIo::open_rw(&self.path).map_err(Error::Corrupt)?;
                index_grow::promote_leaf_directory(&mut io, parent)
            }
            Engine::Fd(lock) => {
                let mut io = lock
                    .lock()
                    .map_err(|_| Error::Corrupt("fd io lock poisoned".into()))?;
                index_grow::promote_leaf_directory(&mut *io, parent)
            }
        }
    }

    fn demote_empty_dir(&self, path: &str) -> Result<bool> {
        match &self.engine {
            Engine::Path(_) => {
                let mut io = PathIo::open_rw(&self.path).map_err(Error::Corrupt)?;
                index_grow::demote_empty_directory(&mut io, path)
            }
            Engine::Fd(lock) => {
                let mut io = lock
                    .lock()
                    .map_err(|_| Error::Corrupt("fd io lock poisoned".into()))?;
                index_grow::demote_empty_directory(&mut *io, path)
            }
        }
    }

    fn rmdir_empty(&self, path: &str) -> Result<()> {
        match self.rmdir(path) {
            Ok(()) => Ok(()),
            Err(e) if index_grow::is_overflow_rmdir_error(&e) => {
                self.demote_empty_dir(path)?;
                self.rmdir(path)
            }
            Err(e) => Err(e),
        }
    }

    fn remove_limited(&self, path: &str, depth: u32, files: &mut u32) -> Result<()> {
        let path = path.trim();
        if path.is_empty() || path == "/" {
            return Err(Error::Unsupported("cannot delete the volume root"));
        }
        if depth > 32 {
            return Err(Error::Unsupported("folder nest too deep"));
        }
        if *files > 8_000 {
            return Err(Error::Unsupported("too many files to delete at once"));
        }
        let st = self.stat(path)?;
        if !st.is_dir {
            *files += 1;
            return self.unlink(path);
        }
        *files += 1;
        let children = self.list(path)?;
        for child in children {
            if child.name == "." || child.name == ".." {
                continue;
            }
            let child_path = join_ntfs(path, &child.name);
            self.remove_limited(&child_path, depth + 1, files)?;
        }
        self.rmdir_empty(path)
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

const NONRES_INITIALIZED_LENGTH: usize = 0x38;

fn bump_data_initialized<T: BlockIo + ?Sized>(io: &mut T, path: &str, end: u64) -> Result<()> {
    if end == 0 {
        return Ok(());
    }
    let rec = read::resolve_path(io, path).map_err(Error::Corrupt)?;
    update_mft_record_io(io, rec, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "unnamed $DATA missing".to_string())?;
        if loc.is_resident {
            return Ok(());
        }
        let off = loc.attr_offset + NONRES_INITIALIZED_LENGTH;
        if off + 8 > record.len() {
            return Err("initialized_length truncated".into());
        }
        let cur = u64::from_le_bytes(record[off..off + 8].try_into().unwrap());
        if end > cur {
            record[off..off + 8].copy_from_slice(&end.to_le_bytes());
        }
        Ok(())
    })
    .map_err(Error::Corrupt)?;
    Ok(())
}

/// Copy `ntfs_path` out of the volume into `dest` using a bounded buffer ring.
/// Directories are copied recursively.
pub fn copy_out(vol: &Volume, ntfs_path: &str, dest: &Path, block: usize) -> Result<CopyStats> {
    let st = vol.stat(ntfs_path)?;
    if st.is_dir {
        return copy_out_dir(vol, ntfs_path, dest, block);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)?;
    let block = block.clamp(256 * 1024, 8 * 1024 * 1024);
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

fn copy_out_dir(vol: &Volume, ntfs_path: &str, dest: &Path, block: usize) -> Result<CopyStats> {
    fs::create_dir_all(dest)?;
    let t0 = Instant::now();
    let mut bytes = 0u64;
    for e in vol.list(ntfs_path)? {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let child_nt = join_ntfs(ntfs_path, &e.name);
        let child_dest = dest.join(&e.name);
        let st = copy_out(vol, &child_nt, &child_dest, block)?;
        bytes += st.bytes;
    }
    Ok(CopyStats {
        bytes,
        elapsed: t0.elapsed(),
    })
}

fn skip_host_name(name: &str) -> bool {
    name.is_empty() || name == ".DS_Store" || name == ".localized" || name.starts_with("._")
}

/// Copy a host file or folder into the NTFS volume (mkdir + create + write).
pub fn copy_in(vol: &Volume, src: &Path, parent: &str, name: &str) -> Result<CopyStats> {
    let mut files = 0u32;
    copy_in_limited(vol, src, parent, name, 0, &mut files)
}

fn copy_in_limited(
    vol: &Volume,
    src: &Path,
    parent: &str,
    name: &str,
    depth: u32,
    files: &mut u32,
) -> Result<CopyStats> {
    vol.require_write()?;
    if depth > 32 {
        return Err(Error::Unsupported("folder nest too deep"));
    }
    if *files > 8_000 {
        return Err(Error::Unsupported("too many files to copy at once"));
    }
    if skip_host_name(name) {
        return Ok(CopyStats {
            bytes: 0,
            elapsed: std::time::Duration::ZERO,
        });
    }
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return Ok(CopyStats {
            bytes: 0,
            elapsed: std::time::Duration::ZERO,
        });
    }
    if meta.is_dir() {
        *files += 1;
        let dest = join_ntfs(parent, name);
        match vol.mkdir(parent, name) {
            Ok(_) => {}
            Err(e) => {
                let exists_dir = vol.stat(&dest).map(|s| s.is_dir).unwrap_or(false);
                if !exists_dir {
                    return Err(e);
                }
            }
        }
        let t0 = Instant::now();
        let mut bytes = 0u64;
        for ent in fs::read_dir(src)? {
            let ent = ent?;
            let fname = ent.file_name().to_string_lossy().into_owned();
            let st = copy_in_limited(vol, &ent.path(), &dest, &fname, depth + 1, files)?;
            bytes += st.bytes;
        }
        return Ok(CopyStats {
            bytes,
            elapsed: t0.elapsed(),
        });
    }
    *files += 1;
    vol.create_file(parent, name)?;
    let dest = join_ntfs(parent, name);
    let t0 = Instant::now();
    let n = copy_in_file_bytes(vol, src, &dest)?;
    Ok(CopyStats {
        bytes: n,
        elapsed: t0.elapsed(),
    })
}

const COPY_IN_STREAM_AFTER: u64 = 1024 * 1024;
const COPY_IN_CHUNK: usize = 4 * 1024 * 1024;

fn copy_in_file_bytes(vol: &Volume, src: &Path, dest: &str) -> Result<u64> {
    let mut f = File::open(src)?;
    let size = f.metadata()?.len();
    if size == 0 {
        return Ok(0);
    }
    if size <= COPY_IN_STREAM_AFTER {
        let mut data = Vec::with_capacity(size as usize);
        f.read_to_end(&mut data)?;
        return vol.write_contents(dest, &data);
    }
    vol.preallocate(dest, size)?;
    let mut offset = 0u64;
    let mut buf = vec![0u8; COPY_IN_CHUNK];
    while offset < size {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        vol.write_at(dest, offset, &buf[..n])?;
        offset += n as u64;
    }
    Ok(offset)
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
    format_filesystem(&mut io, size, 4096, 4096, label, None).map_err(|e| Error::Corrupt(e))?;
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

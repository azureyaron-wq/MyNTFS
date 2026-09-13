//! Linux FUSE front-end (macOS builds a stub).

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Parser;

#[derive(Parser)]
#[command(name = "ntfs-fuse", about = "Mount an NTFS image/device via FUSE (Linux)")]
struct Args {
    source: PathBuf,
    mountpoint: PathBuf,
    #[arg(long)]
    writable: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    #[cfg(not(all(target_os = "linux", feature = "fuse")))]
    {
        let _ = args;
        bail!("ntfs-fuse requires Linux with --features fuse");
    }
    #[cfg(all(target_os = "linux", feature = "fuse"))]
    {
        fuse_main::run(args)
    }
}

#[cfg(all(target_os = "linux", feature = "fuse"))]
mod fuse_main {
    use std::ffi::OsStr;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use fuser::{
        FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
        ReplyStatfs, Request,
    };
    use ntfs_core::WritePolicy;
    use ntfs_vfs::Volume;

    use super::Args;

    struct NtfsFuse {
        vol: Volume,
    }

    pub fn run(args: Args) -> Result<()> {
        let policy = if args.writable {
            WritePolicy::ReadWriteIfSafe
        } else {
            WritePolicy::ReadOnly
        };
        let vol = Volume::mount(&args.source, policy).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let fs = NtfsFuse { vol };
        fuser::mount2(
            fs,
            &args.mountpoint,
            &[
                fuser::MountOption::FSName("myntfs".into()),
                fuser::MountOption::AutoUnmount,
            ],
        )?;
        Ok(())
    }

    impl Filesystem for NtfsFuse {
        fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let parent_path = if parent == 1 { "/".to_string() } else {
                reply.error(libc::ENOENT);
                return;
            };
            let name = name.to_string_lossy();
            if name == "." {
                reply.entry(&Duration::from_secs(1), &dir_attr(1), 0);
                return;
            }
            if name == ".." && parent == 1 {
                reply.entry(&Duration::from_secs(1), &dir_attr(1), 0);
                return;
            }
            let path = format!("{parent_path}{name}");
            match self.vol.stat(&path) {
                Ok(st) => {
                    let ino = st.record.max(2);
                    let attr = if st.is_dir {
                        dir_attr(ino)
                    } else {
                        file_attr(ino, st.size)
                    };
                    reply.entry(&Duration::from_secs(1), &attr, 0);
                }
                Err(_) => reply.error(libc::ENOENT),
            }
        }

        fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
            if ino == 1 {
                reply.attr(&Duration::from_secs(1), &dir_attr(1));
                return;
            }
            reply.error(libc::ENOENT);
        }

        fn read(
            &mut self,
            _req: &Request,
            _ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            reply.error(libc::ENOSYS);
        }

        fn readdir(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            if ino != 1 {
                reply.error(libc::ENOENT);
                return;
            }
            let entries = match self.vol.list("/") {
                Ok(e) => e,
                Err(_) => {
                    reply.error(libc::EIO);
                    return;
                }
            };
            let mut idx = 0i64;
            if offset <= 0 {
                if reply.add(1, idx, FileType::Directory, ".".as_ref()) {
                    reply.ok();
                    return;
                }
                idx += 1;
            }
            if offset <= 1 {
                if reply.add(1, idx, FileType::Directory, "..".as_ref()) {
                    reply.ok();
                    return;
                }
                idx += 1;
            }
            for e in entries {
                if idx < offset {
                    idx += 1;
                    continue;
                }
                let ino = e.record.max(2);
                let kind = if e.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                if reply.add(ino, idx, kind, e.name.as_ref()) {
                    break;
                }
                idx += 1;
            }
            reply.ok();
        }

        fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
            reply.statfs(4 * 1024, 512, 0, 0, 0, 512, 0, 0);
        }
    }

    fn dir_attr(ino: u64) -> FileAttr {
        FileAttr {
            ino,
            size: 4096,
            blocks: 1,
            atime: now(),
            mtime: now(),
            ctime: now(),
            crtime: now(),
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: 501,
            gid: 20,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn file_attr(ino: u64, size: u64) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: (size + 511) / 512,
            atime: now(),
            mtime: now(),
            ctime: now(),
            crtime: now(),
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 501,
            gid: 20,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
    }
}

//! ntfs-cli — portable NTFS tool for macOS and Linux.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use ntfs_core::{VolumeProbe, WritePolicy};
use ntfs_io::{diskarb, FileDevice, RawDevice};
use ntfs_vfs::{
    copy_out, device_gate::DeviceWriteConfirm, format_image, fsck, refuse_block_device_format,
    refuse_block_device_write, Volume,
};

#[derive(Parser)]
#[command(name = "ntfs-cli", about = "MyNTFS portable NTFS read/write tool")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Format a blank image file as NTFS.
    Mkfs {
        path: PathBuf,
        #[arg(long, default_value_t = 64 * 1024 * 1024)]
        size: u64,
        #[arg(long)]
        label: Option<String>,
    },
    /// List directory contents.
    Ls {
        image: PathBuf,
        #[arg(default_value = "/")]
        path: String,
    },
    /// Dump file contents to stdout.
    Cat {
        image: PathBuf,
        path: String,
    },
    /// Copy a file out of the volume.
    Cp {
        image: PathBuf,
        src: String,
        dest: PathBuf,
        #[arg(long, default_value_t = 4 * 1024 * 1024)]
        block: usize,
    },
    /// Stat a path.
    Stat { image: PathBuf, path: String },
    /// Run fsck (clear dirty + reset logfile via am-fs-ntfs).
    Fsck {
        image: PathBuf,
        #[arg(long, hide = true)]
        i_understand_device_write: bool,
    },
    /// Sequential read benchmark.
    Bench {
        image: PathBuf,
        path: String,
        #[arg(long, default_value_t = 4 * 1024 * 1024)]
        block: usize,
    },
    /// Probe safety gates and list NTFS disks (macOS).
    Probe {
        #[arg(long)]
        image: Option<PathBuf>,
    },
    /// Unmount an NTFS volume so MyNTFS can open /dev/rdisk* (macOS).
    /// Disruptive: volume disappears from Finder until remounted.
    Claim {
        bsd: String,
        /// Required for real USB/removable disks — confirms you accept temporary unmount.
        #[arg(long)]
        confirm_unmount: bool,
    },
    /// Write a file into the volume.
    Write {
        image: PathBuf,
        path: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        from: Option<PathBuf>,
        /// Allow mutating a block device (/dev/*). Never use on drives with user data.
        #[arg(long, hide = true)]
        i_understand_device_write: bool,
    },
    /// Create an empty file.
    Touch {
        image: PathBuf,
        parent: String,
        name: String,
        #[arg(long, hide = true)]
        i_understand_device_write: bool,
    },
    /// Create a directory.
    Mkdir {
        image: PathBuf,
        parent: String,
        name: String,
        #[arg(long, hide = true)]
        i_understand_device_write: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Mkfs { path, size, label } => {
            refuse_block_device_format(&path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            format_image(&path, size, label.as_deref())
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("formatted {} ({} bytes)", path.display(), size);
        }
        Command::Ls { image, path } => {
            let vol = open_ro(&image)?;
            for e in vol.list(&path)? {
                let tag = if e.is_dir { "d" } else { "-" };
                println!("{tag} {:>10} {}", e.size, e.name);
            }
        }
        Command::Cat { image, path } => {
            let vol = open_ro(&image)?;
            let st = vol.stat(&path)?;
            let mut buf = vec![0u8; st.size.min(8 * 1024 * 1024) as usize];
            let mut off = 0u64;
            while off < st.size {
                let n = vol.read(&path, off, &mut buf)?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(&mut std::io::stdout(), &buf[..n])?;
                off += n as u64;
            }
        }
        Command::Cp { image, src, dest, block } => {
            let vol = open_ro(&image)?;
            let stats = copy_out(&vol, &src, &dest, block)?;
            println!(
                "copied {} bytes in {:.2?} ({:.1} MB/s)",
                stats.bytes,
                stats.elapsed,
                stats.mbps()
            );
        }
        Command::Stat { image, path } => {
            let vol = open_ro(&image)?;
            let st = vol.stat(&path)?;
            println!(
                "size={} dir={} record={} mtime={}",
                st.size, st.is_dir, st.record, st.mtime_sec
            );
        }
        Command::Fsck {
            image,
            i_understand_device_write,
        } => {
            let confirm = device_confirm(i_understand_device_write);
            fsck(&image, confirm).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("fsck ok: {}", image.display());
        }
        Command::Bench { image, path, block } => {
            let vol = open_ro(&image)?;
            let st = vol.stat(&path)?;
            let mut buf = vec![0u8; block];
            let t0 = Instant::now();
            let mut off = 0u64;
            while off < st.size {
                let n = vol.read(&path, off, &mut buf)?;
                if n == 0 {
                    break;
                }
                off += n as u64;
            }
            let elapsed = t0.elapsed();
            let mbps = (off as f64 / 1_000_000.0) / elapsed.as_secs_f64().max(1e-9);
            println!("read {} bytes in {:.2?} ({:.1} MB/s)", off, elapsed, mbps);
        }
        Command::Probe { image } => {
            if let Some(img) = image {
                let dev = FileDevice::open_ro(&img)?;
                let report = VolumeProbe::probe(&dev)?;
                print_probe(&report);
            } else {
                #[cfg(target_os = "macos")]
                {
                    print_macos_disk_probe()?;
                }
                #[cfg(not(target_os = "macos"))]
                println!("pass --image to probe a file");
            }
        }
        Command::Claim { bsd, confirm_unmount } => {
            if !confirm_unmount {
                bail!(
                    "claim unmounts {bsd} and removes it from Finder; \
                     re-run with --confirm-unmount if you intend that. \
                     Use read-only probe first; never write to USB drives with user data."
                );
            }
            diskarb::unmount_and_claim(&bsd)?;
            let rd = diskarb::rdisk_path(&bsd);
            println!("unmounted {bsd}; raw path {rd}");
            println!("{}", diskarb::macos_raw_access_hint());
            println!("read-only test: ntfs-cli ls {rd} /");
            println!("remount: diskutil mount {bsd}");
        }
        Command::Write {
            image,
            path,
            content,
            from,
            i_understand_device_write,
        } => {
            let confirm = device_confirm(i_understand_device_write);
            refuse_block_device_write(&image, confirm).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let vol = open_rw(&image, confirm)?;
            let data = if let Some(c) = content {
                c.into_bytes()
            } else if let Some(f) = from {
                std::fs::read(f)?
            } else {
                bail!("provide --content or --from");
            };
            let n = vol.write_contents(&path, &data)?;
            println!("wrote {n} bytes to {path}");
        }
        Command::Touch {
            image,
            parent,
            name,
            i_understand_device_write,
        } => {
            let confirm = device_confirm(i_understand_device_write);
            refuse_block_device_write(&image, confirm).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let vol = open_rw(&image, confirm)?;
            let rec = vol.create_file(&parent, &name)?;
            println!("created record {rec} at {parent}/{name}");
        }
        Command::Mkdir {
            image,
            parent,
            name,
            i_understand_device_write,
        } => {
            let confirm = device_confirm(i_understand_device_write);
            refuse_block_device_write(&image, confirm).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let vol = open_rw(&image, confirm)?;
            let rec = vol.mkdir(&parent, &name)?;
            println!("mkdir record {rec} at {parent}/{name}");
        }
    }
    Ok(())
}

fn open_ro(path: &Path) -> Result<Volume> {
    open(path, WritePolicy::ReadOnly, DeviceWriteConfirm::default())
}

fn open_rw(path: &Path, confirm: DeviceWriteConfirm) -> Result<Volume> {
    open(path, WritePolicy::ReadWriteIfSafe, confirm)
}

fn open(path: &Path, policy: WritePolicy, confirm: DeviceWriteConfirm) -> Result<Volume> {
    let s = path.to_string_lossy();
    if s.starts_with("/dev/") {
        let writable = matches!(policy, WritePolicy::ReadWriteIfSafe | WritePolicy::ForceWrite);
        let _dev = RawDevice::open(path, writable).context("open raw device")?;
    }
    Volume::mount_with_confirm(path, policy, confirm).map_err(|e| anyhow::anyhow!(e.to_string()))
}

fn device_confirm(flag: bool) -> DeviceWriteConfirm {
    DeviceWriteConfirm {
        allow_block_device: flag,
    }
}

fn print_probe(report: &ntfs_core::SafetyReport) {
    println!(
        "verified={} dirty={} hibernated={} bitlocker={} efs={}",
        report.verified, report.dirty, report.hibernated, report.bitlocker, report.efs_present
    );
    for r in &report.reasons {
        println!("  - {r}");
    }
    println!(
        "writable (safe policy): {}",
        report.writable(WritePolicy::ReadWriteIfSafe)
    );
}

#[cfg(target_os = "macos")]
fn print_macos_disk_probe() -> Result<()> {
    let disks = diskarb::list_ntfs_bsd_names()?;
    if disks.is_empty() {
        println!("no NTFS disks found");
        return Ok(());
    }
    println!("{}", diskarb::macos_raw_access_hint());
    println!("USB testing: read-only ls/stat/cat/cp only; never write to user data.");
    for bsd in disks {
        let info = diskarb::disk_summary(&bsd)?;
        let rd = diskarb::rdisk_path(&bsd);
        let raw_ok = diskarb::can_open_rdisk_ro(&bsd);
        println!("---");
        println!("bsd: {bsd}  rdisk: {rd}");
        if let Some(n) = &info.volume_name {
            println!("volume: {n}");
        }
        if let Some(fs) = &info.file_system {
            println!("filesystem: {fs}");
        }
        if let Some(m) = &info.mount_point {
            println!("mount: {m} (mounted={})", info.mounted.unwrap_or(false));
        } else {
            println!("mount: (not mounted)");
        }
        println!("raw read access: {}", if raw_ok { "yes" } else { "no" });
        if raw_ok {
            if let Ok(dev) = RawDevice::open(&rd, false) {
                if let Ok(report) = VolumeProbe::probe(&dev) {
                    print_probe(&report);
                }
            }
        }
    }
    Ok(())
}

//! FD-backed mount: I/O must keep working after the original path is gone.

use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use ntfs_core::{BlockDevice, WritePolicy};
use ntfs_vfs::{format_image, DeviceWriteConfirm, Volume};

fn formatted_image(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "myntfs-fd-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("FdIo")).unwrap();
    img
}

#[test]
fn mount_from_fd_after_path_unlinked() {
    let img = formatted_image("unlinked");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&img)
        .unwrap();
    std::fs::remove_file(&img).unwrap();
    assert!(!img.exists());

    let vol = Volume::mount_from_file(
        file,
        &img,
        WritePolicy::ReadWriteIfSafe,
        DeviceWriteConfirm::default(),
    )
    .unwrap();
    assert!(vol.writable());

    vol.create_file("/", "fd.txt").unwrap();
    vol.write_contents("/fd.txt", b"owned-fd").unwrap();
    vol.sync().unwrap();
    vol.rename("/fd.txt", "renamed.txt").unwrap();
    let mut buf = [0u8; 16];
    let n = vol.read("/renamed.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"owned-fd");
    vol.unlink("/renamed.txt").unwrap();
    let names: Vec<_> = vol.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(!names.iter().any(|n| n == "renamed.txt" || n == "fd.txt"));
}

#[test]
fn mount_from_raw_fd_dups_and_survives_close() {
    let img = formatted_image("dup");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&img)
        .unwrap();
    let fd = file.as_raw_fd();
    let vol = Volume::mount_from_raw_fd(
        fd,
        &img,
        WritePolicy::ReadWriteIfSafe,
        DeviceWriteConfirm::default(),
    )
    .unwrap();
    drop(file);
    vol.create_file("/", "after-close.txt").unwrap();
    vol.write_contents("/after-close.txt", b"still-here").unwrap();
    let mut buf = [0u8; 16];
    let n = vol.read("/after-close.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"still-here");
}

#[test]
fn read_only_fd_rejects_write_mount() {
    let img = formatted_image("ro");
    let file = OpenOptions::new().read(true).open(&img).unwrap();
    let err = match Volume::mount_from_raw_fd(
        file.as_raw_fd(),
        &img,
        WritePolicy::ReadWriteIfSafe,
        DeviceWriteConfirm::default(),
    ) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected write mount on a read-only fd to fail"),
    };
    assert!(
        err.contains("write denied") || err.contains("not writable"),
        "got {err}"
    );
}

#[test]
fn invalid_fd_fails_cleanly() {
    let err = match Volume::mount_from_raw_fd(
        -1,
        "/tmp/nope",
        WritePolicy::ReadOnly,
        DeviceWriteConfirm::default(),
    ) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected invalid fd to fail"),
    };
    assert!(err.contains("i/o") || err.contains("Bad file"), "got {err}");
}

#[test]
fn mkdir_via_fd_with_simulated_sector_alignment() {
    let img = formatted_image("mkdir-align");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&img)
        .unwrap();
    let vol = Volume::mount_from_file_with_logical_block_size(
        file,
        &img,
        WritePolicy::ReadWriteIfSafe,
        DeviceWriteConfirm::default(),
        512,
    )
    .unwrap();
    vol.mkdir("/", "AlignedMkdir").unwrap();
    let names: Vec<_> = vol.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.iter().any(|n| n == "AlignedMkdir"));
    vol.rmdir("/AlignedMkdir").unwrap();
}

#[test]
fn fd_roundtrip_with_simulated_sector_alignment() {
    let img = formatted_image("align-roundtrip");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&img)
        .unwrap();
    let vol = Volume::mount_from_file_with_logical_block_size(
        file,
        &img,
        WritePolicy::ReadWriteIfSafe,
        DeviceWriteConfirm::default(),
        512,
    )
    .unwrap();
    vol.create_file("/", "fd.txt").unwrap();
    vol.write_contents("/fd.txt", b"owned-fd").unwrap();
    vol.rename("/fd.txt", "renamed.txt").unwrap();
    let mut buf = [0u8; 16];
    let n = vol.read("/renamed.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"owned-fd");
    vol.unlink("/renamed.txt").unwrap();
}

/// Live USB roundtrip for the owned-FD engine. Opt-in:
/// `MYNTFS_USB_RDISK=/dev/rdiskNsM cargo test -p ntfs-vfs --test fd_io usb_live_rdisk -- --ignored`
/// Refuses whole disks, non-rdisk paths, and devices larger than 32 GiB.
#[test]
#[ignore]
fn usb_live_rdisk_create_delete() {
    use std::os::unix::fs::FileTypeExt;

    let path = std::env::var("MYNTFS_USB_RDISK").expect("MYNTFS_USB_RDISK unset");
    assert!(
        path.starts_with("/dev/rdisk") && path.contains('s'),
        "refusing {path}: expected /dev/rdiskNsM slice"
    );
    let meta = std::fs::metadata(&path).unwrap();
    assert!(
        meta.file_type().is_char_device(),
        "{path} is not a character device"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let sized = ntfs_io::FileDevice::from_file(file.try_clone().unwrap(), false, &path).unwrap();
    let bytes = sized.size();
    drop(sized);
    assert!(
        bytes > 1_000_000_000 && bytes < 32 * 1024 * 1024 * 1024,
        "refusing live write: device size {bytes} is not a small test stick"
    );
    let vol = Volume::mount_from_file(
        file,
        &path,
        WritePolicy::ReadWriteIfSafe,
        DeviceWriteConfirm {
            allow_block_device: true,
        },
    )
    .unwrap();
    assert!(vol.writable(), "volume did not come up writable");

    let tag = format!(
        "MyNtfsSmoke{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    vol.mkdir("/", &tag).unwrap();
    let file_name = format!("{tag}.txt");
    vol.create_file(&format!("/{tag}"), &file_name).unwrap();
    vol.write_contents(&format!("/{tag}/{file_name}"), b"standalone-ok")
        .unwrap();
    let mut buf = [0u8; 16];
    let n = vol
        .read(&format!("/{tag}/{file_name}"), 0, &mut buf)
        .unwrap();
    assert_eq!(&buf[..n], b"standalone-ok");
    vol.unlink(&format!("/{tag}/{file_name}")).unwrap();
    vol.rmdir(&format!("/{tag}")).unwrap();
}

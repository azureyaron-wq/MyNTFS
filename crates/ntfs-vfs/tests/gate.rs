//! Mount safety: unverified probe must deny write.

use std::path::PathBuf;

use ntfs_core::WritePolicy;
use ntfs_vfs::{format_image, Volume};

#[test]
fn unverified_image_denies_write_mount() {
    let dir = std::env::temp_dir().join("myntfs-gate");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img: PathBuf = dir.join("zeros.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    let msg = match Volume::mount(&img, WritePolicy::ReadWriteIfSafe) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected write mount to fail on unverified image"),
    };
    assert!(
        msg.contains("write denied"),
        "expected WriteDenied, got {msg}"
    );
}

#[test]
fn formatted_image_allows_rw() {
    let dir = std::env::temp_dir().join("myntfs-gate-rw");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img: PathBuf = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("Gate")).unwrap();
    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    assert!(vol.safety().verified);
    assert!(vol.writable());
}

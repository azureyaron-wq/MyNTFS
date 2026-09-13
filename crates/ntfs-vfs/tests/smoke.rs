//! End-to-end smoke test: format, write, read via am-fs-ntfs engine.

use std::path::PathBuf;

use ntfs_core::WritePolicy;
use ntfs_vfs::{format_image, Volume};

#[test]
fn format_write_read_roundtrip() {
    let dir = std::env::temp_dir().join("myntfs-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img: PathBuf = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("Smoke")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    vol.create_file("/", "hello.txt").unwrap();
    vol.write_contents("/hello.txt", b"MyNTFS smoke test").unwrap();
    vol.mkdir("/", "docs").unwrap();
    vol.write_contents("/docs/readme.txt", b"nested").unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadOnly).unwrap();
    let entries = vol.list("/").unwrap();
    assert!(entries.iter().any(|e| e.name == "hello.txt"));
    assert!(entries.iter().any(|e| e.name == "docs"));

    let mut buf = [0u8; 64];
    let n = vol.read("/hello.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"MyNTFS smoke test");
}

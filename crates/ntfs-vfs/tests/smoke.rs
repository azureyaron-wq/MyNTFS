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
    vol.write_contents("/hello.txt", b"MyNTFS smoke test")
        .unwrap();
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

#[test]
fn copy_in_nested_folder_skips_ds_store() {
    use ntfs_vfs::copy_in;

    let dir = std::env::temp_dir().join(format!("myntfs-copyin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("incoming/sub")).unwrap();
    std::fs::write(dir.join("incoming/a.txt"), b"alpha").unwrap();
    std::fs::write(dir.join("incoming/sub/b.txt"), b"beta").unwrap();
    std::fs::write(dir.join("incoming/.DS_Store"), b"junk").unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("CopyIn")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    copy_in(&vol, &dir.join("incoming"), "/", "incoming").unwrap();

    let root: Vec<_> = vol.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(root.contains(&"incoming".into()));
    let top: Vec<_> = vol
        .list("/incoming")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(top.contains(&"a.txt".into()));
    assert!(top.contains(&"sub".into()));
    assert!(!top.contains(&".DS_Store".into()));
    let mut buf = [0u8; 8];
    let n = vol.read("/incoming/sub/b.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"beta");
}

#[test]
fn mkdir_then_many_files_promotes_index_root() {
    let dir = std::env::temp_dir().join(format!("myntfs-indexgrow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("IdxGrow")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    vol.mkdir("/", "drop").unwrap();
    // Fresh mkfs images only have ~40 free MFT records, so keep this
    // under that. Long names fill $INDEX_ROOT well before then and
    // force the $INDEX_ALLOCATION promotion this test is for. Stay
    // inside one 4096-byte INDX so the folder stays Windows-readable.
    const N: usize = 16;
    let pad = "x".repeat(72);
    let names: Vec<String> = (0..N).map(|i| format!("n{i:03}_{pad}.txt")).collect();
    for (i, name) in names.iter().enumerate() {
        vol.create_file("/drop", name)
            .unwrap_or_else(|e| panic!("create {i} ({name}): {e}"));
        vol.write_contents(&format!("/drop/{name}"), format!("body-{i}").as_bytes())
            .unwrap();
    }
    let listed: Vec<_> = vol
        .list("/drop")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(listed.len(), N, "listed {listed:?}");
    for name in &names {
        assert!(listed.contains(name), "missing {name}");
    }
    let last = names.last().unwrap();
    let mut buf = [0u8; 16];
    let n = vol.read(&format!("/drop/{last}"), 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], format!("body-{}", N - 1).as_bytes());
}

#[test]
fn remove_deletes_nested_files_and_folders() {
    let dir = std::env::temp_dir().join(format!("myntfs-rmtree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("RmTree")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    vol.mkdir("/", "tree").unwrap();
    vol.mkdir("/tree", "sub").unwrap();
    vol.create_file("/tree/sub", "a.txt").unwrap();
    vol.write_contents("/tree/sub/a.txt", b"nested").unwrap();
    vol.create_file("/tree", "b.txt").unwrap();
    vol.write_contents("/tree/b.txt", b"top").unwrap();

    vol.remove("/tree").unwrap();
    let root: Vec<_> = vol.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(
        !root.contains(&"tree".into()),
        "root still has tree: {root:?}"
    );
}

#[test]
fn remove_promoted_directory_after_many_files() {
    let dir = std::env::temp_dir().join(format!("myntfs-rmpromo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("RmPromo")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    vol.mkdir("/", "drop").unwrap();
    let pad = "x".repeat(72);
    for i in 0..16 {
        let name = format!("n{i:03}_{pad}.txt");
        vol.create_file("/drop", &name).unwrap();
        vol.write_contents(&format!("/drop/{name}"), b"x").unwrap();
    }
    vol.remove("/drop").unwrap();
    let root: Vec<_> = vol.list("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(
        !root.contains(&"drop".into()),
        "root still has drop: {root:?}"
    );
}

#[test]
fn commit_for_unmount_succeeds_after_writes() {
    let dir = std::env::temp_dir().join(format!("myntfs-unmount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("Unmount")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    vol.create_file("/", "hello.txt").unwrap();
    vol.write_contents("/hello.txt", b"windows").unwrap();
    vol.commit_for_unmount().unwrap();
    let mut buf = [0u8; 16];
    let n = vol.read("/hello.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"windows");
}

#[test]
fn drop_readonly_volume_does_not_panic() {
    let dir = std::env::temp_dir().join(format!("myntfs-drop-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("DropRo")).unwrap();
    let vol = Volume::mount(&img, WritePolicy::ReadOnly).unwrap();
    drop(vol);
}

#[test]
fn copy_in_streams_large_file_and_copy_out_folder() {
    use ntfs_vfs::{copy_in, copy_out};

    let dir = std::env::temp_dir().join(format!("myntfs-stream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("incoming")).unwrap();
    let payload = vec![0x5A_u8; 5_000_000];
    std::fs::write(dir.join("incoming/big.bin"), &payload).unwrap();
    std::fs::write(dir.join("incoming/tiny.txt"), b"ok").unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("Stream")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    copy_in(&vol, &dir.join("incoming"), "/", "incoming").unwrap();
    let st = vol.stat("/incoming/big.bin").unwrap();
    assert_eq!(st.size, payload.len() as u64, "stat size");
    let mut buf = vec![0u8; payload.len()];
    let n = vol.read("/incoming/big.bin", 0, &mut buf).unwrap();
    assert_eq!(n, payload.len(), "read len");
    if let Some(i) = buf.iter().zip(payload.iter()).position(|(a, b)| a != b) {
        panic!("mismatch at {i}: got {:#x} want {:#x}", buf[i], payload[i]);
    }

    let out = dir.join("exported");
    copy_out(&vol, "/incoming", &out, 256 * 1024).unwrap();
    assert_eq!(std::fs::read(out.join("big.bin")).unwrap(), payload);
    assert_eq!(std::fs::read(out.join("tiny.txt")).unwrap(), b"ok");

    let (total, free) = vol.volume_space().unwrap();
    assert!(total > 0);
    assert!(free > 0);
    assert!(free < total);
}

/// First data run is small; a second file steals the next clusters so grow
/// must add another run and resize `$DATA` (the old 8-byte mapping slot).
#[test]
fn grow_resizes_mapping_pairs_when_runs_split() {
    let dir = std::env::temp_dir().join(format!("myntfs-grow-split-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("vol.img");
    format_image(&img, 64 * 1024 * 1024, Some("GrowSplit")).unwrap();

    let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
    vol.create_file("/", "a.bin").unwrap();
    vol.write_contents("/a.bin", &vec![0x11_u8; 8192]).unwrap();
    vol.create_file("/", "block.bin").unwrap();
    vol.write_contents("/block.bin", &vec![0x22_u8; 2_000_000])
        .unwrap();
    vol.grow("/a.bin", 6_000_000).unwrap();
    let payload = vec![0x33_u8; 100_000];
    vol.write_at("/a.bin", 0, &payload).unwrap();
    vol.write_at("/a.bin", 5_000_000, b"tail").unwrap();

    let st = vol.stat("/a.bin").unwrap();
    assert_eq!(st.size, 6_000_000);
    let mut buf = vec![0u8; 100_000];
    let n = vol.read("/a.bin", 0, &mut buf).unwrap();
    assert_eq!(n, 100_000);
    assert_eq!(buf, payload);
    let mut tail = [0u8; 4];
    let n = vol.read("/a.bin", 5_000_000, &mut tail).unwrap();
    assert_eq!(&tail[..n], b"tail");
}

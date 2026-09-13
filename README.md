# MyNTFS

Native macOS + Linux NTFS read/write stack built on a permissive Rust engine.

## Layout

- `crates/ntfs-core` — clean-room parsers, safety probes, `BlockDevice` trait
- `crates/ntfs-io` — image/raw device I/O, DiskArbitration helpers (macOS)
- `crates/ntfs-vfs` — mounted volume API (wraps `am-fs-ntfs` for read/write)
- `crates/ntfs-ffi` — C ABI (`myntfs_*`) for Swift / FSKit
- `crates/ntfs-cli` — portable CLI (`mkfs`, `ls`, `cat`, `cp`, `bench`, …)
- `crates/ntfs-fuse` — Linux FUSE front-end (`--features fuse`)
- `apple/MyNTFS` — SwiftUI dedicated app
- `apple/MyNTFSModule` — FSKit extension skeleton (Phase 5)

## Quick start

```bash
cargo build --release
./target/release/ntfs-cli mkfs testdata/vol.img --size 67108864 --label MyNTFS
./target/release/ntfs-cli touch testdata/vol.img / hello.txt
./target/release/ntfs-cli write testdata/vol.img /hello.txt --content "Hello NTFS"
./target/release/ntfs-cli ls testdata/vol.img /
```

Golden images:

```bash
bash scripts/build-golden-images.sh
```

### USB / real disk testing

Use **read-only** commands only on drives with user data (`ls`, `stat`, `cat`, `cp` out to your Mac). The CLI refuses writes to `/dev/*` unless you pass a hidden `--i-understand-device-write` flag (do not use on the Moked drive).

```bash
./target/debug/ntfs-cli probe          # lists NTFS disks, mount state, raw access
# Optional: unmount for engine test (disruptive — remount with diskutil mount diskNsY)
./target/debug/ntfs-cli claim disk7s1 --confirm-unmount
./target/debug/ntfs-cli ls /dev/rdisk7s1 /
```

Raw `/dev/rdisk*` access requires membership in the macOS `operator` group or `sudo`.

## macOS app

```bash
bash apple/MyNTFS/build.sh
open apple/MyNTFS.app
```

The dedicated app works today without special entitlements. Open NTFS disk images or `/dev/rdiskNsY` after unmounting with `ntfs-cli claim`.

## Phase 5 — FSKit extension (gated)

System-wide auto-mount requires an FSKit File System Extension (`apple/MyNTFSModule/`).

| Requirement | Detail |
|---|---|
| Entitlement | `com.apple.developer.fskit.fsmodule` + mandatory `com.apple.security.app-sandbox` |
| Paid account | Provisioning-profile gated; free Apple IDs cannot sign the entitlement |
| Local dev | Ad-hoc sign + boot-arg `amfi_get_out_of_my_way=1` (see OpenZFS FSKit PoC) |
| Mount | `sudo mount -F -t myntfs /dev/diskNsY /Volumes/Label` |
| Probe order | `FSProbeOrder` 500 beats Apple's read-only `ntfs.fs` (1000–4000) |
| Performance | Implement KOIO (`blockmapFile` / `completeIO`); inhibit for compressed/resident data |

Apple's built-in NTFS on macOS 26 is userspace FSKit and **read-only** (`FSImplementation = UserFS` only). MyNTFS targets write via `am-fs-ntfs` behind KOIO.

After building the appex in Xcode:

```bash
bash apple/MyNTFSModule/dev-install.sh /path/to/MyNTFSModule.appex
```


MIT OR Apache-2.0. The write engine uses [`am-fs-ntfs`](https://crates.io/crates/am-fs-ntfs) (MIT/Apache). No GPL NTFS code is linked.

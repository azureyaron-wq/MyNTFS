# MyNTFS

Native macOS + Linux NTFS read/write stack built on a permissive Rust engine.

**License:** [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) (`SPDX-License-Identifier: MIT OR Apache-2.0`).

The write engine uses [`am-fs-ntfs`](https://crates.io/crates/am-fs-ntfs) (MIT/Apache). No GPL NTFS code is linked.

## Clone and test

```bash
git clone https://github.com/azureyaron-wq/MyNTFS.git
cd MyNTFS
cargo test -p ntfs-core -p ntfs-io -p ntfs-vfs
```

The git tree is **source only**. Do not expect `target/`, `graphify-out/`, or a prebuilt Mac app in the clone.

## Layout

- `crates/ntfs-core` — clean-room parsers, safety probes, `BlockDevice` trait
- `crates/ntfs-io` — image/raw device I/O, DiskArbitration helpers (macOS)
- `crates/ntfs-vfs` — mounted volume API (wraps `am-fs-ntfs` for read/write)
- `crates/ntfs-ffi` — C ABI (`myntfs_*`) for Swift / FSKit
- `crates/ntfs-cli` — portable CLI (`mkfs`, `ls`, `cat`, `cp`, `bench`, …)
- `crates/ntfs-fuse` — Linux FUSE front-end (`--features fuse`; needs `libfuse3-dev` on Ubuntu)
- `apple/MyNTFS` — SwiftUI dedicated app **sources** (build locally)

## Quick start (CLI / images)

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

Use **read-only** commands only on drives with user data (`ls`, `stat`, `cat`, `cp` out to your Mac). The CLI refuses writes to `/dev/*` unless you pass a hidden `--i-understand-device-write` flag.

```bash
./target/debug/ntfs-cli probe          # lists NTFS disks, mount state, raw access
# Optional: unmount for engine test (disruptive — remount with diskutil mount diskNsY)
./target/debug/ntfs-cli claim disk7s1 --confirm-unmount
./target/debug/ntfs-cli ls /dev/rdisk7s1 /
```

Raw `/dev/rdisk*` access requires membership in the macOS `operator` group or `sudo`.

## macOS app (not shipped in git)

Build the desktop app from this checkout:

```bash
bash apple/MyNTFS/build.sh
open apple/MyNTFS.app
```

`apple/MyNTFS.app` is a **local build artifact**. It is gitignored and is not a notarized GitHub Release. Use a disposable USB stick for write tests.

The dedicated app works today without special entitlements. Open NTFS disk images, or Enable writes on an external NTFS USB (macOS will ask for your password each time).

## Advanced — FSKit extension (lab only)

System-wide auto-mount is **not** the v1 product. `apple/MyNTFSModule/` is an experimental FSKit skeleton.

| Requirement | Detail |
|---|---|
| Entitlement | `com.apple.developer.fskit.fsmodule` + mandatory `com.apple.security.app-sandbox` |
| Paid account | Provisioning-profile gated; free Apple IDs cannot sign the entitlement |
| Local lab | Ad-hoc sign; AMFI boot-args are **lab-only**, not a supported install |
| Mount | `sudo mount -F -t myntfs /dev/diskNsY /Volumes/Label` |

Apple's built-in NTFS on macOS 26 is userspace FSKit and **read-only**. Do not treat AMFI-off or this appex as the normal MyNTFS install.

After building the appex in Xcode:

```bash
bash apple/MyNTFSModule/dev-install.sh /path/to/MyNTFSModule.appex
```

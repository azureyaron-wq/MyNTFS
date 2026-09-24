# MyNTFS

Open-source, permissive NTFS **explorer + explicit USB write** for Mac/Linux — local app/CLI, no GPL driver stack; **experimental writes**, not a notarized Finder automount product.

> **Experimental / pre-1.0.** Write support can **corrupt or destroy** volume data. Backup first. Use **disposable USB sticks** for write tests. The software is provided **AS IS**, without warranty of any kind — see [LICENSE-MIT](LICENSE-MIT) or [LICENSE-APACHE](LICENSE-APACHE).
>
> This is **not** a notarized Finder automount replacement for Paragon/Tuxera. There are **no** official binary GitHub Releases yet. Clone and **build from source**.

NTFS is a trademark of Microsoft Corporation. MyNTFS is an independent project and is **not affiliated with, endorsed by, or sponsored by Microsoft**.

**License:** [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) (`SPDX-License-Identifier: MIT OR Apache-2.0`). See also [NOTICE](NOTICE) and [docs/IP.md](docs/IP.md).

The write engine uses [`am-fs-ntfs`](https://crates.io/crates/am-fs-ntfs) (MIT/Apache). No GPL NTFS code is linked.

## Clone and test

```bash
git clone https://github.com/azureyaron-wq/MyNTFS.git
cd MyNTFS
cargo test --workspace
```

The git tree is **source only**. Do not expect `target/`, `graphify-out/`, or a prebuilt Mac app in the clone.

## Layout

- `crates/ntfs-core` — parsers, safety probes, `BlockDevice` trait
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

Writes are **experimental**. Prefer a blank / disposable USB. After a crash or a failed write session, run Windows `chkdsk`. See [SECURITY.md](SECURITY.md).

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

`apple/MyNTFS.app` is a **local build artifact**. It is gitignored, **ad-hoc signed** (`codesign -s -`), and is **not** a Gatekeeper-trusted or notarized GitHub Release. Use a disposable USB stick for write tests.

The dedicated app works today without special entitlements. Open NTFS disk images, or Enable writes on an **external** NTFS USB (macOS will ask for your password each time). Finder staying empty during Enable writes is exclusive access, not a bypass.

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

## Legal

- Dual license: MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE))
- Third-party attribution: [NOTICE](NOTICE)
- Interoperability / IP notes: [docs/IP.md](docs/IP.md)
- Vulnerability reports: [SECURITY.md](SECURITY.md)
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md)

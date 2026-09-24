# Contributing

Thanks for looking at MyNTFS. The project is pre-1.0: experimental NTFS
**explorer + explicit USB write**, not a notarized Finder automount product.

## Build and test

```bash
cargo test --workspace
```

Linux FUSE extras:

```bash
# Ubuntu: sudo apt-get install -y libfuse3-dev pkg-config
cargo test -p ntfs-core -p ntfs-io -p ntfs-vfs
cargo build -p ntfs-fuse --features fuse
```

macOS app (local artifact only, ad-hoc signed):

```bash
bash apple/MyNTFS/build.sh
```

## USB write tests

Use **disposable media only**. Never run Enable writes or
`--i-understand-device-write` against a disk with irreplaceable data.

After a failed or interrupted write session, run Windows `chkdsk`. See
[SECURITY.md](SECURITY.md) for non-journaled `$LogFile` behavior and grow
limits.

## Pull requests

- Keep changes focused. Prefer tests (`crates/ntfs-vfs/tests/smoke.rs` and
  crate unit tests) for engine behavior.
- Do not reintroduce osascript elevation, DYLD interpose, AMFI-off as an
  install path, or a read-only elevated FD fallback.
- Do not add GitHub Release binaries, `.app` artifacts, secrets, USB dumps,
  or `target/` output.
- Match existing dual license: MIT OR Apache-2.0.
- CI (`.github/workflows/ci.yml`) must stay green: `test-macos` and
  `test-linux`.

## Vulnerabilities

Do not file a public issue for unfixed disk-write or privilege bugs. Follow
[SECURITY.md](SECURITY.md) (email **azureyaron@gmail.com**).

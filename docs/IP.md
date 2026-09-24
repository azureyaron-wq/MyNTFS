# Intellectual property notes

MyNTFS is an **independent** implementation for NTFS **interoperability**
(explore volumes; optional explicit write to external USB). It is not a
Microsoft product.

## Sources

On-disk parsers in `crates/ntfs-core` are written against **public** NTFS
layout documentation and observed on-disk structures. The read/write engine
wraps the open, dual-licensed crate [`am-fs-ntfs`](https://crates.io/crates/am-fs-ntfs)
(MIT OR Apache-2.0). See [NOTICE](../NOTICE).

This project does **not** use Microsoft confidential sources and does **not**
include decompiled Windows NTFS driver code.

## Trademarks

NTFS is a trademark of Microsoft Corporation. MyNTFS is **not affiliated
with, endorsed by, or sponsored by Microsoft**. See the README disclaimer.

## License

Contributions are accepted under the same dual license as the rest of the
tree: MIT OR Apache-2.0 (`SPDX-License-Identifier: MIT OR Apache-2.0`).

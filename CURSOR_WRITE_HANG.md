# Cursor handoff — MyNTFS New File / mftbm EINVAL (local-first)

Date: 2026-09-13  
From: Clive + **Orchard (Apple)** confirmation. Aegis/Canary/Keystone/Pulse prior write-path consensus still applies for DA/authopen. **No Atlas.**

## Stop / ownership
Clive **stopped** local `build.sh` per user request. Cursor owns finish: verify patch, rebuild `.app`, USB retest, commit/PR if desired.

## User symptoms
1. Alert: `corrupt: read mftbm: i/o: Invalid argument (os error 22)`
2. Green **Read-Write** badge + `$MFT` listing, mutate buttons look grey — **New File did run**; buttons are not the create blocker (Orchard).

## Root cause (CONFIRMED)
`crates/ntfs-io/src/lib.rs` — `detect_logical_block_size(file, meta_len)` returned **0 whenever `meta_len > 0`**. Callers pass `device_file_size()` (ioctl size, tens of GB on `/dev/rdisk*`), so alignment bounce **never armed**.  
`am-fs-ntfs` create → 1-byte `$MFT:$Bitmap` `read_exact_at` → macOS raw `pread` → **EINVAL**.

Listing can work (larger transfers); create always hits 1-byte mftbm.

## Fix (already applied on Mac working tree — not pushed)
File: `crates/ntfs-io/src/lib.rs`

- Key off **`file.metadata().len()`** (0 on rdisk), **not** ioctl device size.
- If stat size is 0: `DKIOCGETBLOCKSIZE`, else fallback **512** (never leave 0).
- Unit test added: `detect_uses_stat_not_device_size` — **passed** (`cargo test -p ntfs-io --lib` → 10/10 ok).

Git: `master` dirty — `M crates/ntfs-io/src/lib.rs` only (uncommitted). Remote `origin` is https://github.com/azureyaron-wq/MyNTFS (private). Cursor cloud agent may need GitHub **access** grant for that repo.

## Cursor EXECUTE
1. Confirm the patched `detect_logical_block_size` in `crates/ntfs-io/src/lib.rs` matches the contract above.
2. `cargo test -p ntfs-io --lib`
3. `bash apple/MyNTFS/build.sh` — test the **built** `apple/MyNTFS.app`
4. USB: Enable writes → **New File** / save — must not show mftbm EINVAL
5. Image path regress: `.img` Enable writes / New File — no password
6. Optional: clearer error if EINVAL on raw I/O (“alignment”) instead of generic corrupt
7. Commit + push/PR when green (do not ship `target/` or `.app`)

## Do not regress
- DA hold for whole RW session (`dahold.c`)
- `authopen` O_RDWR + `myntfs_mount_fd` (not AppleScript privopen / `mount_ex` on USB path)
- Slice-only force-unmount; external-only

## Suggested commit message
```
fix(ntfs-io): detect rdisk logical block size via stat, not ioctl size

device_file_size() made detect_logical_block_size always return 0 on
/dev/rdisk*, so 1-byte $MFT:$Bitmap reads hit EINVAL (mftbm).
```

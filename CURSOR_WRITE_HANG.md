# Cursor FINAL handoff — MyNTFS USB write (once and for all)

Date: 2026-09-13  
From: Clive roll-up of **Aegis + Canary + Keystone + Pulse**. **No Atlas. No Forge.**

## Consensus verdict
**v1 write path that ships:**

1. **DA mount veto + claim** (`dahold.c` / `myntfs_da_hold`) — hold **before** auth and keep alive until close/fail/cancel  
2. **Force-unmount allowlisted slice only** → verify `Mounted: No`  
3. **`authopen` O_RDWR** (`authopen_fd.c` / `myntfs_authopen_rdisk`) + F_GETFL writable + no RO fallback  
4. **`myntfs_mount_ex(..., writable=1, allow_device_write=1)`** + `myntfs_is_writable`  
5. **External-only** refuse (boot/internal/`disk0`) at DA **and** claim layers  
6. **Consent + busy/cancel** → on fail/cancel: clear priv FD, `da_release`, remount Finder RO  

**Do not regress** to AppleScript / `privopen` / root `open` via osascript.  
**Do not gate v1** on SMJobBless, FSKit-as-primary product, or default whole-disk `unmountDisk force`.  
Helper-unmount alone is **not** enough for production (Canary/Aegis/Keystone agree).

Tree already has `authopen_fd.c` + `dahold.c` (privopen gone). Cursor job = finish wiring, verify order, pass G0–G10, then UI P0s.

## Critical order (Keystone + Aegis)
`veto/hold early → Authorization/authopen → force-unmount immediately before O_RDWR (or re-unmount after auth) → set priv fd → mount_ex`  

DA hold must **not** drop during the password sheet. Fail closed if still mounted after hold.

## Aegis must-nots
- Whole-disk force by default  
- Shell/`system()`/osascript elevation  
- Soft claim Ok while still mounted  
- Softening “still mounted” into retry-without-hold  
- Probe fail-open writable; FSKit Phase 5 probe-all until validated  
- Ship `target/`  

## Canary must-pass gates (v1)
| Gate | Expect |
|------|--------|
| **G0** Image Enable writes / New File | No password |
| **G1** USB happy path | DA hold OK → authopen writable → RW badge → create/edit/delete |
| **G2** Wait ≥7s on password | Still RW under DA veto, or explicit fail — never generic EPERM only |
| **G3** Parallel `diskutil mount` during elevate | Success under veto or clear fail ≤2 retries; busy clears; Finder remount on fail |
| **G4** Still mounted after hold | Fail, no elevate |
| **G5** Non-writable fd | Reject; no RO→RW mount |
| **G6** New File ≤30s after RW | Off-main FFI; UI responsive |
| **G7** Cancel/fail | da_release, clear priv, Finder RO, no hang |
| **G8** Dirty/hiber/BitLocker | Safety message, not “permission failed” |
| **G9** Internal/boot | Refused |
| **G10** Slice-only force | `diskNsM` only by default |

## Pulse UI (after write green, or parallel P0)
**P0:** Wire or delete dead `showWriteConfirm` / `showPicker`; add Export log + Close volume; badge “Finder (read-only)”; Cancel must not pretend to stop FFI mutations (disable Cancel during mutate or document); status always visible.  
**P1:** Split overcrowded toolbar; safety bullets in Enable alert; disable mutate with help until `canMutate`.  
**P2:** Strip `agentDbg` / localhost debug ingest from release; sync Mac as source of truth.

## Keystone extras
- Strip agentDbg HTTP/file ingest from release builds  
- Keep rdisk allowlist + safety gates  
- Defer SMJobBless + FSKit product path + heavy UI polish until write G0–G10 green  


## Canary R1–R12 (must-pass vs defer)

**One-liner:** v1 USB write = DA hold + authopen O_RDWR + slice force-unmount, proven by R1–R3/R8–R11 (+ light R5/R10); **not** unmount-in-helper alone.

| Race | v1 | Notes |
|------|----|-------|
| R1 remount between unmount and open | **MUST** | DA veto + hold-before-authopen; storm test |
| R2 soft claim Ok while mounted | **MUST** | Fail closed if Mounted Yes |
| R3 auth-sheet remount (F7) | **MUST** | Wait ≥7s on password |
| R4 whole-disk vs slice | **MUST (slice-only)** | Whole-disk not default |
| R5 double consumer after RW | **MUST (light)** | Create works after RW badge |
| R6 Paragon / 3rd-party | DEFER | Explicit error if hold fails |
| R7 sleep/lid during password | DEFER | Cancel/retry OK |
| R8 post-open remount before mutate | **MUST** | New File ≤30s after Enable writes |
| R9 stale UI mountPoint | **MUST** | Trust diskutil/DA, not UI cache |
| R10 Enable-writes retry storm | **MUST (light)** | No hang; remount-on-fail |
| R11 image control | **MUST** | Always first; no password |
| R12 cable yank mid-elevate | DEFER | Fail-closed |

Residual accept for v1 (don’t block ship): Paragon exotic, sleep/wake mid-auth, multi-instance, cable-yank — fail-closed UX, not extra architecture.

## Rebuild
`bash apple/MyNTFS/build.sh` — test the **built** `.app`.

## Suggested commit
```
fix(macos): DA hold + authopen RDWR for FSKit-safe USB writes

Keep mount veto through auth; slice force-unmount; writable fd only;
no osascript privopen. Gates G0–G10.
```

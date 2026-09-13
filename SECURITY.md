# Security policy

## Supported versions

`master` is the only supported line while the project is pre-1.0. There are no
notarized GitHub Releases yet; treat local `apple/MyNTFS.app` builds as
developer artifacts, not a supported install channel.

## Threat model (v1)

MyNTFS is a **local USB write tool** for **external, removable NTFS volumes**.

- Writes to `/dev/rdiskNsM` slices go through DiskArbitration exclusive hold
  plus a **one-shot** `authopen` prompt. Authorization is not cached across
  Enable writes sessions.
- The engine refuses internal / boot disks and whole-disk (non-slice) paths.
- Images are a separate path; they must not open raw USB devices.
- Finder staying empty during read-write is exclusive access, not a bypass.

Out of scope for v1: passwordless helpers (`SMAppService` / `SMJobBless`),
FSKit as a product mount, and shipping a notarized `.app`.

**Not a supported path:** disabling AMFI (`amfi_get_out_of_my_way=1`),
osascript-as-root, or any “always allow” coaching. Those are lab-only notes
for the optional FSKit experiment, not normal install.

## Reporting a vulnerability

Email **azureyaron@gmail.com** with:

- affected commit or build date
- whether the report is about the Rust engine, the macOS app, or docs
- a repro that does **not** require writing a disk with irreplaceable data

Please do not open a public issue for unfixed disk-write or privilege bugs.
We will acknowledge receipt and follow up on a fix or a wontfix with rationale.

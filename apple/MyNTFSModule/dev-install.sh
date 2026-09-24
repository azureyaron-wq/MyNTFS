#!/usr/bin/env bash
# Install a locally-built MyNTFS FSKit extension for paid-account / lab testing.
#
# Preferred: paid Apple Developer `com.apple.developer.fskit.fsmodule` entitlement.
#   - Xcode 16.3+ with an appex target (File System Extension)
#   - Paid Apple Developer account
#   - Rust static lib: bash ../MyNTFS/build.sh (or cargo build --release -p ntfs-ffi)
#
# DANGEROUS / lab-only / never consumer install:
#   Ad-hoc signing with Recovery boot-arg amfi_get_out_of_my_way=1 disables AMFI
#   protections system-wide. Not a preferred path. Not end-user setup.
#
# Usage:
#   ./dev-install.sh /path/to/MyNTFSModule.appex
set -euo pipefail

APPEX="${1:-}"
if [[ -z "$APPEX" || ! -d "$APPEX" ]]; then
  echo "usage: $0 /path/to/MyNTFSModule.appex" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")" && pwd)"
ENT="$ROOT/MyNTFSModule.entitlements"
DEST="/Library/ExtensionKit/Extensions/$(basename "$APPEX")"

echo "Signing (ad-hoc)…"
codesign -f -s - --entitlements "$ENT" "$APPEX"

echo "Installing to $DEST (requires sudo)…"
sudo mkdir -p "$(dirname "$DEST")"
sudo rm -rf "$DEST"
sudo cp -R "$APPEX" "$DEST"
sudo codesign -f -s - --entitlements "$ENT" "$DEST"

echo "Registering with pluginkit…"
sudo pluginkit -a "$DEST"

echo "Restarting fskitd…"
sudo killall fskitd 2>/dev/null || true

echo "Done. Enable the module in System Settings → Login Items & Extensions → File System Extensions."
echo "Mount with: sudo mount -F -t myntfs /dev/diskNsY /Volumes/Label"
echo "List modules: pluginkit -m -v -p com.apple.fskit.fsmodule"

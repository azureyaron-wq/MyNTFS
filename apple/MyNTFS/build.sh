#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
export CARGO_TARGET_DIR="$ROOT/target"

if ! command -v cargo >/dev/null 2>&1; then
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust first:" >&2
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  echo "Then restart your terminal or run: source \"\$HOME/.cargo/env\"" >&2
  exit 1
fi

echo "Building Rust FFI…"
cargo build --release -p ntfs-ffi

LIB="$ROOT/target/release/libmyntfs.a"
HDR="$ROOT/crates/ntfs-ffi/include/myntfs.h"
OUT="$ROOT/apple/MyNTFS.app"
SRC="$ROOT/apple/MyNTFS/Sources"

rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS" "$OUT/Contents/Resources"

cat > "$OUT/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleExecutable</key><string>MyNTFS</string>
  <key>CFBundleIdentifier</key><string>com.myntfs.app</string>
  <key>CFBundleName</key><string>MyNTFS</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSRemovableVolumesUsageDescription</key>
  <string>MyNTFS needs access to your USB drive so it can read and write the NTFS volume.</string>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key>
      <string>Disk Image</string>
      <key>CFBundleTypeRole</key>
      <string>Viewer</string>
      <key>LSHandlerRank</key>
      <string>Alternate</string>
      <key>CFBundleTypeExtensions</key>
      <array>
        <string>img</string>
        <string>dmg</string>
        <string>raw</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

echo "Compiling disk access helpers…"
clang -c -O2 -Wall -target arm64-apple-macosx15.0 \
  -I "$ROOT/crates/ntfs-ffi/include" \
  -o "$ROOT/target/dahold.o" \
  "$ROOT/apple/MyNTFS/dahold.c"
clang -c -O2 -Wall -target arm64-apple-macosx15.0 \
  -I "$ROOT/crates/ntfs-ffi/include" \
  -o "$ROOT/target/authopen_fd.o" \
  "$ROOT/apple/MyNTFS/authopen_fd.c"

echo "Compiling SwiftUI app…"
swiftc \
  -O \
  -parse-as-library \
  -target arm64-apple-macosx15.0 \
  -sdk "$(xcrun --show-sdk-path)" \
  -import-objc-header "$HDR" \
  -I "$ROOT/crates/ntfs-ffi/include" \
  "$SRC/MyNTFSApp.swift" \
  "$LIB" \
  "$ROOT/target/dahold.o" \
  "$ROOT/target/authopen_fd.o" \
  -o "$OUT/Contents/MacOS/MyNTFS" \
  -framework SwiftUI \
  -framework AppKit \
  -framework UniformTypeIdentifiers \
  -framework DiskArbitration \
  -framework CoreFoundation \
  -framework Security \
  -framework IOKit

codesign -s - --force "$OUT" 2>/dev/null || true
echo "Built $OUT"

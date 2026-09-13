#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export CARGO_TARGET_DIR="$ROOT/target"
mkdir -p testdata/golden
CLI="$ROOT/target/debug/ntfs-cli"

cargo build -p ntfs-cli

mk() {
  local name="$1" size="$2" label="${3:-MyNTFS}"
  local img="$ROOT/testdata/golden/${name}.img"
  "$CLI" mkfs "$img" --size "$size" --label "$label"
  echo "created $img"
}

mk basic 67108864 "Basic"
mk unicode 67108864 "Unicode"

# Populate basic volume
RW="$ROOT/target/debug/ntfs-cli"
"$RW" touch testdata/golden/basic.img / hello.txt
"$RW" write testdata/golden/basic.img /hello.txt --content "Hello NTFS from MyNTFS"
"$RW" mkdir testdata/golden/basic.img / sub
"$RW" write testdata/golden/basic.img /sub/readme.txt --content "nested"
"$RW" touch testdata/golden/basic.img / "世界.txt"
"$RW" write testdata/golden/basic.img "/世界.txt" --content "unicode"

"$CLI" ls testdata/golden/basic.img /
"$CLI" cat testdata/golden/basic.img /hello.txt

echo "Golden corpus ready under testdata/golden/"

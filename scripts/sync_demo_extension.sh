#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
src_so="$repo_root/target/debug/libgodot_network_audio.so"
dst_dir="$repo_root/example/addons/godot_network_audio/bin"
dst_so="$dst_dir/godot_network_audio.so"

if [[ ! -f "$src_so" ]]; then
  echo "missing built extension: $src_so" >&2
  echo "run: source ~/.cargo/env && cargo build -p godot_network_audio" >&2
  exit 1
fi

mkdir -p "$dst_dir"
cp "$src_so" "$dst_so"
echo "$dst_so"

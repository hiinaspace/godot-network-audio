#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)

src_audio="$repo_root/target/debug/libgodot_network_audio.so"
src_iroh="$repo_root/target/debug/libgodot_network_audio_iroh_gdext.so"

dst_audio_dir="$repo_root/example_iroh/addons/godot_network_audio/bin"
dst_iroh_dir="$repo_root/example_iroh/addons/godot_network_audio_iroh/bin"

if [[ ! -f "$src_audio" ]]; then
  echo "missing built extension: $src_audio" >&2
  exit 1
fi

if [[ ! -f "$src_iroh" ]]; then
  echo "missing built extension: $src_iroh" >&2
  exit 1
fi

mkdir -p "$dst_audio_dir" "$dst_iroh_dir"
cp "$src_audio" "$dst_audio_dir/godot_network_audio.so"
cp "$src_iroh" "$dst_iroh_dir/godot_network_audio_iroh_gdext.so"

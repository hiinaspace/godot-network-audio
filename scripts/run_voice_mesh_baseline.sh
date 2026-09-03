#!/usr/bin/env bash
# Run the current-version direct/full-mesh baseline matrix.
# Usage: run_voice_mesh_baseline.sh [OUTPUT_DIR] [SECONDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/baseline-current}"
RUN_SECONDS="${2:-10}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"

mkdir -p "$OUTPUT_DIR"

if [[ ! -x "$BINARY" ]]; then
  cargo build --release -p voice-mesh-bench
fi

{
  date --iso-8601=seconds
  git -C "$ROOT_DIR" rev-parse HEAD
  rustc --version
  cargo --version
  uname -a
  printf 'cpu.max='
  cat /sys/fs/cgroup/cpu.max 2>/dev/null || true
  printf 'memory.max='
  cat /sys/fs/cgroup/memory.max 2>/dev/null || true
} >"$OUTPUT_DIR/environment.txt"
cargo tree -p voice-mesh-bench --depth 1 >"$OUTPUT_DIR/dependencies.txt"

cp /sys/fs/cgroup/cpu.stat "$OUTPUT_DIR/cpu-before.txt" 2>/dev/null || true

for participants in 4 8 16 32; do
  for talkers in 1 2; do
    for dtx in on off; do
      name="${participants}p-${talkers}t-dtx-${dtx}"
      echo "running $name (${RUN_SECONDS}s)"
      "$BINARY" \
        --participants "$participants" \
        --talkers "$talkers" \
        --seconds "$RUN_SECONDS" \
        --dtx "$dtx" \
        --output "$OUTPUT_DIR/$name.json" \
        >"$OUTPUT_DIR/$name.stdout"
    done
  done
done

cp /sys/fs/cgroup/cpu.stat "$OUTPUT_DIR/cpu-after.txt" 2>/dev/null || true
python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

#!/usr/bin/env bash
# Run correlated game-interest stress profiles on the direct sender-filtered path.
# Usage: run_voice_interest_stress.sh [OUTPUT_DIR] [SECONDS] [SEEDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/interest-stress}"
RUN_SECONDS="${2:-36}"
SEEDS="${3:-5}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
PARTICIPANTS="${PARTICIPANTS:-32}"
TALKERS="${TALKERS:-4}"
INTEREST_LISTENERS="${INTEREST_LISTENERS:-7}"
RECEIVER_POLICY="${RECEIVER_POLICY:-pool}"
PROFILES="${PROFILES:-rotating crowd-burst group-merge boundary-oscillation}"
RUNTIME_WORKERS="${RUNTIME_WORKERS:-8}"

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

for profile in $PROFILES; do
  for seed in $(seq 1 "$SEEDS"); do
    name="${PARTICIPANTS}p-${TALKERS}t-${INTEREST_LISTENERS}i-${profile}-${RECEIVER_POLICY}-seed${seed}"
    echo "running $name (${RUN_SECONDS}s)"
    "$BINARY" \
      --scenario game-interest \
      --participants "$PARTICIPANTS" \
      --talkers "$TALKERS" \
      --interest-listeners "$INTEREST_LISTENERS" \
      --delivery sender-filtered \
      --receiver-policy "$RECEIVER_POLICY" \
      --interest-profile "$profile" \
      --seconds "$RUN_SECONDS" \
      --dtx on \
      --seed "$seed" \
      --runtime-workers "$RUNTIME_WORKERS" \
      --output "$OUTPUT_DIR/$name.json" \
      >"$OUTPUT_DIR/$name.stdout"
  done
done

cp /sys/fs/cgroup/cpu.stat "$OUTPUT_DIR/cpu-after.txt" 2>/dev/null || true
python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

#!/usr/bin/env bash
# Run the first game-shaped direct-delivery comparison.
# Usage: run_voice_game_interest.sh [OUTPUT_DIR] [SECONDS] [SEEDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/game-interest}"
RUN_SECONDS="${2:-30}"
SEEDS="${3:-5}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
PARTICIPANT_COUNTS="${PARTICIPANT_COUNTS:-8 16 32}"
RECEIVER_POLICY="${RECEIVER_POLICY:-retire}"

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

for participants in $PARTICIPANT_COUNTS; do
  interest_listeners=$((participants / 4 - 1))
  if ((interest_listeners < 3)); then
    interest_listeners=3
  fi
  for delivery in sender-filtered broadcast-discard; do
    for seed in $(seq 1 "$SEEDS"); do
      name="${participants}p-4t-${interest_listeners}i-${delivery}-${RECEIVER_POLICY}-seed${seed}"
      echo "running $name (${RUN_SECONDS}s)"
      "$BINARY" \
        --scenario game-interest \
        --participants "$participants" \
        --talkers 4 \
        --interest-listeners "$interest_listeners" \
        --delivery "$delivery" \
        --receiver-policy "$RECEIVER_POLICY" \
        --seconds "$RUN_SECONDS" \
        --dtx on \
        --seed "$seed" \
        --output "$OUTPUT_DIR/$name.json" \
        >"$OUTPUT_DIR/$name.stdout"
    done
  done
done

cp /sys/fs/cgroup/cpu.stat "$OUTPUT_DIR/cpu-after.txt" 2>/dev/null || true
python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

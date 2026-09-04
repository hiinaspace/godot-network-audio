#!/usr/bin/env bash
# Compare co-located virtual clients in one process versus independent processes.
# Usage: run_voice_process_comparison.sh [OUTPUT_DIR] [SECONDS] [SEEDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/process-layout}"
RUN_SECONDS="${2:-12}"
SEEDS="${3:-3}"
SEED_START="${SEED_START:-1}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
RUNTIME_WORKERS="${RUNTIME_WORKERS:-8}"
TALKERS="${TALKERS:-4}"
INTEREST_LISTENERS="${INTEREST_LISTENERS:-7}"
PARTICIPANT_COUNTS="${PARTICIPANT_COUNTS:-8 16 32}"
TOPOLOGIES="${TOPOLOGIES:-direct star}"
PROCESS_LAYOUTS="${PROCESS_LAYOUTS:-single multi}"

mkdir -p "$OUTPUT_DIR"
cargo build --release -p voice-mesh-bench

{
  date --iso-8601=seconds
  git -C "$ROOT_DIR" rev-parse HEAD
  rustc --version
  cargo --version
  uname -a
} >"$OUTPUT_DIR/environment.txt"

run_case() {
  local participants="$1"
  local topology="$2"
  local layout="$3"
  local seed="$4"
  local run_name="${participants}p-${topology}-${layout}-seed${seed}"
  echo "running $run_name (${RUN_SECONDS}s)"
  "$BINARY" \
    --process-layout "$layout" \
    --topology "$topology" \
    --scenario game-interest \
    --participants "$participants" \
    --talkers "$TALKERS" \
    --interest-listeners "$INTEREST_LISTENERS" \
    --interest-profile rotating \
    --delivery sender-filtered \
    --receiver-policy pool \
    --seconds "$RUN_SECONDS" \
    --dtx on \
    --seed "$seed" \
    --runtime-workers "$RUNTIME_WORKERS" \
    --output "$OUTPUT_DIR/$run_name.json" \
    >"$OUTPUT_DIR/$run_name.stdout"
}

seed_end=$((SEED_START + SEEDS - 1))
for participants in $PARTICIPANT_COUNTS; do
  for seed in $(seq "$SEED_START" "$seed_end"); do
    for topology in $TOPOLOGIES; do
      for layout in $PROCESS_LAYOUTS; do
        run_case "$participants" "$topology" "$layout" "$seed"
      done
    done
  done
done

python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

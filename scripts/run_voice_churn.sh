#!/usr/bin/env bash
# Close and reconnect one participant's real Iroh connections during media.
# Usage: run_voice_churn.sh [OUTPUT_DIR] [SECONDS] [SEEDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/churn}"
RUN_SECONDS="${2:-12}"
SEEDS="${3:-3}"
SEED_START="${SEED_START:-1}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
PARTICIPANTS="${PARTICIPANTS:-8}"
RUNTIME_WORKERS="${RUNTIME_WORKERS:-8}"
CHURN_PARTICIPANT="${CHURN_PARTICIPANT:-0}"
CHURN_START_MS="${CHURN_START_MS:-4000}"

mkdir -p "$OUTPUT_DIR"
cargo build --release -p voice-mesh-bench

{
  date --iso-8601=seconds
  git -C "$ROOT_DIR" rev-parse HEAD
  rustc --version
  cargo --version
  uname -a
} >"$OUTPUT_DIR/environment.txt"

# Every participant sends continuously so transport gaps are not confounded by
# DTX or changing game-interest membership.
run_case() {
  local name="$1"
  local seed="$2"
  local churn="$3"
  local downtime_ms="$4"
  local run_name="${PARTICIPANTS}p-${name}-seed${seed}"
  echo "running $run_name (${RUN_SECONDS}s)"
  "$BINARY" \
    --scenario baseline \
    --participants "$PARTICIPANTS" \
    --talkers "$PARTICIPANTS" \
    --seconds "$RUN_SECONDS" \
    --dtx off \
    --churn "$churn" \
    --churn-participant "$CHURN_PARTICIPANT" \
    --churn-start-ms "$CHURN_START_MS" \
    --churn-downtime-ms "$downtime_ms" \
    --seed "$seed" \
    --runtime-workers "$RUNTIME_WORKERS" \
    --output "$OUTPUT_DIR/$run_name.json" \
    >"$OUTPUT_DIR/$run_name.stdout"
}

seed_end=$((SEED_START + SEEDS - 1))
for seed in $(seq "$SEED_START" "$seed_end"); do
  run_case clean "$seed" none 1
  run_case reconnect-250ms "$seed" reconnect 250
  run_case reconnect-1000ms "$seed" reconnect 1000
  run_case reconnect-3000ms "$seed" reconnect 3000
done

python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

#!/usr/bin/env bash
# Exercise NetEq/playout with deterministic impairment after Iroh receipt.
# This does not test Iroh under loss; use run_voice_transport_netem.sh for that.
# Usage: run_voice_media_impairment.sh [OUTPUT_DIR] [SECONDS] [SEEDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/media-impairment}"
RUN_SECONDS="${2:-12}"
SEEDS="${3:-3}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
PARTICIPANTS="${PARTICIPANTS:-8}"
TALKERS="${TALKERS:-4}"
INTEREST_LISTENERS="${INTEREST_LISTENERS:-7}"
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
} >"$OUTPUT_DIR/environment.txt"

# name mode loss-percent burst-ms outage-start-ms outage-duration-ms
PROFILES=(
  "clean          none          0  60 3000    0"
  "uniform-1      uniform-loss  1  60 3000    0"
  "uniform-3      uniform-loss  3  60 3000    0"
  "uniform-5      uniform-loss  5  60 3000    0"
  "burst-1-30ms   burst-loss    1  30 3000    0"
  "burst-3-60ms   burst-loss    3  60 3000    0"
  "burst-5-120ms  burst-loss    5 120 3000    0"
  "outage-100ms   outage        0  60 3000  100"
  "outage-300ms   outage        0  60 3000  300"
  "outage-1000ms  outage        0  60 3000 1000"
)

for profile_spec in "${PROFILES[@]}"; do
  read -r name mode loss_percent burst_ms outage_start_ms outage_duration_ms \
    <<<"$profile_spec"
  for seed in $(seq 1 "$SEEDS"); do
    run_name="${PARTICIPANTS}p-${TALKERS}t-${name}-seed${seed}"
    echo "running $run_name (${RUN_SECONDS}s)"
    "$BINARY" \
      --scenario game-interest \
      --participants "$PARTICIPANTS" \
      --talkers "$TALKERS" \
      --interest-listeners "$INTEREST_LISTENERS" \
      --delivery sender-filtered \
      --receiver-policy pool \
      --interest-profile rotating \
      --media-impairment "$mode" \
      --media-loss-percent "$loss_percent" \
      --media-burst-ms "$burst_ms" \
      --media-outage-start-ms "$outage_start_ms" \
      --media-outage-duration-ms "$outage_duration_ms" \
      --seconds "$RUN_SECONDS" \
      --dtx on \
      --seed "$seed" \
      --runtime-workers "$RUNTIME_WORKERS" \
      --output "$OUTPUT_DIR/$run_name.json" \
      >"$OUTPUT_DIR/$run_name.stdout"
  done
done

python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

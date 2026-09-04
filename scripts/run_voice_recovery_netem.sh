#!/usr/bin/env bash
# Exercise game-shaped voice under static and clean-impaired-clean Iroh paths.
# Usage: run_voice_recovery_netem.sh [OUTPUT_DIR] [SECONDS] [SEEDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/recovery-netem}"
RUN_SECONDS="${2:-24}"
SEEDS="${3:-3}"
SEED_START="${SEED_START:-1}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
PARTICIPANTS="${PARTICIPANTS:-8}"
TALKERS="${TALKERS:-4}"
INTEREST_LISTENERS="${INTEREST_LISTENERS:-7}"
RUNTIME_WORKERS="${RUNTIME_WORKERS:-8}"
IMPAIRMENT_START_SECONDS="${IMPAIRMENT_START_SECONDS:-6}"
IMPAIRMENT_SECONDS="${IMPAIRMENT_SECONDS:-8}"
RUN_PID=""

mkdir -p "$OUTPUT_DIR"
sudo -n true

initial_qdisc="$(sudo tc qdisc show dev lo)"
if [[ "$initial_qdisc" != qdisc\ noqueue* ]]; then
  echo "refusing to replace unexpected loopback qdisc: $initial_qdisc" >&2
  exit 1
fi

remove_netem() {
  sudo tc qdisc del dev lo root 2>/dev/null || true
}

cleanup() {
  if [[ -n "$RUN_PID" ]] && kill -0 "$RUN_PID" 2>/dev/null; then
    kill "$RUN_PID" 2>/dev/null || true
    wait "$RUN_PID" 2>/dev/null || true
  fi
  remove_netem
}
trap cleanup EXIT

cargo build --release -p voice-mesh-bench

{
  date --iso-8601=seconds
  git -C "$ROOT_DIR" rev-parse HEAD
  rustc --version
  cargo --version
  uname -a
  printf 'initial_qdisc=%s\n' "$initial_qdisc"
  printf 'impairment_start_seconds=%s\n' "$IMPAIRMENT_START_SECONDS"
  printf 'impairment_seconds=%s\n' "$IMPAIRMENT_SECONDS"
} >"$OUTPUT_DIR/environment.txt"

run_benchmark() {
  local run_name="$1"
  local seed="$2"
  "$BINARY" \
    --scenario game-interest \
    --participants "$PARTICIPANTS" \
    --talkers "$TALKERS" \
    --interest-listeners "$INTEREST_LISTENERS" \
    --delivery sender-filtered \
    --receiver-policy pool \
    --interest-profile rotating \
    --seconds "$RUN_SECONDS" \
    --dtx on \
    --seed "$seed" \
    --runtime-workers "$RUNTIME_WORKERS" \
    --output "$OUTPUT_DIR/$run_name.json" \
    >"$OUTPUT_DIR/$run_name.stdout"
}

verify_qdisc_packets() {
  local profile="$1"
  local qdisc_path="$2"
  python3 - "$profile" "$qdisc_path" <<'PY'
import re
import sys

profile, path = sys.argv[1:]
qdisc = open(path, encoding="utf-8").read()
match = re.search(r"Sent \d+ bytes (\d+) pkt", qdisc)
if not match or int(match.group(1)) == 0:
    raise SystemExit(f"netem profile {profile} counted no packets")
print(f"verified {profile}: shaped_packets={match.group(1)}")
PY
}

run_static() {
  local name="$1"
  local seed="$2"
  shift 2
  local run_name="${PARTICIPANTS}p-${TALKERS}t-${name}-seed${seed}"
  remove_netem
  if (( $# > 0 )); then
    sudo tc qdisc add dev lo root netem "$@"
  fi
  echo "running $run_name (${RUN_SECONDS}s)"
  run_benchmark "$run_name" "$seed"
  if (( $# > 0 )); then
    sudo tc -s qdisc show dev lo >"$OUTPUT_DIR/$run_name-qdisc.txt"
    verify_qdisc_packets "$run_name" "$OUTPUT_DIR/$run_name-qdisc.txt"
  fi
  remove_netem
}

run_recovery() {
  local name="$1"
  local seed="$2"
  shift 2
  local run_name="${PARTICIPANTS}p-${TALKERS}t-${name}-seed${seed}"
  remove_netem
  echo "running $run_name (${RUN_SECONDS}s; clean→impaired→clean)"
  run_benchmark "$run_name" "$seed" &
  RUN_PID=$!
  sleep "$IMPAIRMENT_START_SECONDS"
  kill -0 "$RUN_PID"
  printf 'impairment_added_at=%s\n' "$(date --iso-8601=ns)" \
    >"$OUTPUT_DIR/$run_name-transitions.txt"
  sudo tc qdisc add dev lo root netem "$@"
  sleep "$IMPAIRMENT_SECONDS"
  kill -0 "$RUN_PID"
  sudo tc -s qdisc show dev lo >"$OUTPUT_DIR/$run_name-qdisc.txt"
  printf 'impairment_removed_at=%s\n' "$(date --iso-8601=ns)" \
    >>"$OUTPUT_DIR/$run_name-transitions.txt"
  remove_netem
  wait "$RUN_PID"
  RUN_PID=""
  verify_qdisc_packets "$run_name" "$OUTPUT_DIR/$run_name-qdisc.txt"
}

seed_end=$((SEED_START + SEEDS - 1))
for seed in $(seq "$SEED_START" "$seed_end"); do
  run_static clean "$seed"
  run_static delay40j10 "$seed" delay 40ms 10ms distribution normal
  run_static delay80j30 "$seed" delay 80ms 30ms distribution normal
  run_recovery recovery-delay-loss "$seed" \
    delay 60ms 20ms distribution normal loss 1%
  run_recovery recovery-delay-burst "$seed" \
    delay 40ms 10ms distribution normal loss gemodel 1% 33.333% 100%
done

final_qdisc="$(sudo tc qdisc show dev lo)"
if [[ "$final_qdisc" != qdisc\ noqueue* ]]; then
  echo "loopback qdisc did not return to noqueue: $final_qdisc" >&2
  exit 1
fi
trap - EXIT

python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

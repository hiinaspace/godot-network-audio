#!/usr/bin/env bash
# Exercise the Iroh/QUIC path with loopback tc netem and verify qdisc counters.
# Usage: run_voice_transport_netem.sh [OUTPUT_DIR] [SECONDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/voice-mesh/transport-netem}"
RUN_SECONDS="${2:-12}"
BINARY="$ROOT_DIR/target/release/voice-mesh-bench"
PARTICIPANTS="${PARTICIPANTS:-8}"
TALKERS="${TALKERS:-2}"
RUNTIME_WORKERS="${RUNTIME_WORKERS:-8}"

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
trap remove_netem EXIT

cargo build --release -p voice-mesh-bench

{
  date --iso-8601=seconds
  git -C "$ROOT_DIR" rev-parse HEAD
  rustc --version
  cargo --version
  uname -a
  printf 'initial_qdisc=%s\n' "$initial_qdisc"
} >"$OUTPUT_DIR/environment.txt"

# name minimum-expected-latency-ms netem arguments
PROFILES=(
  "clean       0  "
  "delay40j10 20  delay 40ms 10ms distribution normal"
  "loss1       0  loss 1%"
  "loss3       0  loss 3%"
  "burst3      0  loss gemodel 1% 33.333% 100%"
)

for profile_spec in "${PROFILES[@]}"; do
  read -r name minimum_latency_ms netem_args <<<"$profile_spec"
  remove_netem
  if [[ -n "${netem_args:-}" ]]; then
    # Word splitting is intentional: the profile table contains tc arguments.
    # shellcheck disable=SC2086
    sudo tc qdisc add dev lo root netem $netem_args
  fi

  run_name="${PARTICIPANTS}p-${TALKERS}t-${name}"
  echo "running $run_name (${RUN_SECONDS}s)"
  "$BINARY" \
    --scenario baseline \
    --participants "$PARTICIPANTS" \
    --talkers "$TALKERS" \
    --seconds "$RUN_SECONDS" \
    --dtx on \
    --runtime-workers "$RUNTIME_WORKERS" \
    --output "$OUTPUT_DIR/$run_name.json" \
    >"$OUTPUT_DIR/$run_name.stdout"

  sudo tc -s qdisc show dev lo >"$OUTPUT_DIR/$run_name-qdisc.txt"
  python3 - "$name" "$minimum_latency_ms" "$OUTPUT_DIR/$run_name.json" \
    "$OUTPUT_DIR/$run_name-qdisc.txt" <<'PY'
import json
import re
import sys

name, minimum_latency_ms, result_path, qdisc_path = sys.argv[1:]
result = json.load(open(result_path, encoding="utf-8"))
qdisc = open(qdisc_path, encoding="utf-8").read()

if name != "clean":
    match = re.search(r"Sent \d+ bytes (\d+) pkt", qdisc)
    if not match or int(match.group(1)) == 0:
        raise SystemExit(f"netem profile {name} counted no packets")
if result["latency_us_p50"] < float(minimum_latency_ms) * 1000:
    raise SystemExit(
        f"profile {name} median latency {result['latency_us_p50']}us did not reflect netem"
    )
print(
    f"verified {name}: latency_p50={result['latency_us_p50']}us "
    f"missing={result['missing_datagrams']}"
)
PY
done

remove_netem
final_qdisc="$(sudo tc qdisc show dev lo)"
if [[ "$final_qdisc" != qdisc\ noqueue* ]]; then
  echo "loopback qdisc did not return to noqueue: $final_qdisc" >&2
  exit 1
fi
trap - EXIT

python3 "$ROOT_DIR/scripts/summarize_voice_mesh.py" "$OUTPUT_DIR" \
  >"$OUTPUT_DIR/summary.csv"
echo "summary: $OUTPUT_DIR/summary.csv"

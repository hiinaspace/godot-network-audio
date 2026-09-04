#!/usr/bin/env bash
# Deterministic Godot receive-stream churn against lightweight Rust peers.
# Usage: run_godot_voice_churn.sh [OUTPUT_DIR] [PEERS] [ACTIVE] [PHASE_SECONDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="$(realpath -m "${1:-$ROOT_DIR/target/godot-voice-churn}")"
PEERS="${2:-31}"
ACTIVE="${3:-7}"
PHASE_SECONDS="${4:-3}"
EXPECTED_SECONDS="$(python3 -c "print(float('$PHASE_SECONDS') * 5.0 + 2.0)")"
RECEIVER_SAFETY_SECONDS=120
GODOT_BIN="${GODOT_BIN:-$(command -v godot || command -v godot4 || true)}"
RECEIVER_CPUSET="${GNA_CHURN_RECEIVER_CPUSET:-}"
LOADGEN_CPUSET="${GNA_CHURN_LOADGEN_CPUSET:-}"
LOADGEN_NICE="${GNA_CHURN_LOADGEN_NICE:-0}"

mkdir -p "$OUTPUT_DIR"
runtime_dir="$(mktemp -d "${TMPDIR:-/tmp}/gna-churn.XXXXXX")"
if [[ -f "$HOME/.cargo/env" ]]; then source "$HOME/.cargo/env"; fi
cargo build -p godot_network_audio --features iroh-transport >/dev/null
cargo build --release -p voice-mesh-bench --bin godot_voice_churn >/dev/null
bash "$ROOT_DIR/scripts/sync_iroh_example_extensions.sh" >/dev/null

tag="$$"
sink="gna_churn_output_${tag}"
module_id=""
receiver_pid=""
capture_pid=""
loadgen_pid=""
sampler_pid=""
cleanup() {
  set +e
  for pid in "$loadgen_pid" "$receiver_pid" "$capture_pid" "$sampler_pid"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
  done
  [[ -n "$module_id" ]] && pactl unload-module "$module_id" 2>/dev/null || true
  rm -rf -- "$runtime_dir"
}
trap cleanup EXIT

module_id="$(pactl load-module module-null-sink sink_name="$sink" rate=48000)"
ffmpeg -hide_banner -loglevel error -nostdin -y -f pulse -i "$sink.monitor" \
  -t "$RECEIVER_SAFETY_SECONDS" -ac 2 -ar 48000 "$OUTPUT_DIR/mixed_output.wav" \
  >"$OUTPUT_DIR/output_capture.log" 2>&1 &
capture_pid=$!

endpoint="$OUTPUT_DIR/receiver_endpoint.json"
trace="$runtime_dir/receiver_trace.jsonl"
receiver_events="$OUTPUT_DIR/receiver_events.jsonl"
quit_file="$OUTPUT_DIR/receiver_done_${tag}"
receiver_prefix=()
if [[ -n "$RECEIVER_CPUSET" ]]; then
  receiver_prefix=(taskset -c "$RECEIVER_CPUSET")
fi
env PULSE_SINK="$sink" GNA_IROH_ROLE=receiver GNA_IROH_BIND_ADDR=127.0.0.1:0 \
  GNA_IROH_ENDPOINT_INFO_PATH="$endpoint" GNA_DEMO_OUTPUT_DEVICE="$sink" \
  GNA_DEMO_SPATIALIZE="${GNA_CHURN_SPATIALIZE:-1}" \
  GNA_DEMO_PRINT_STATS=0 \
  GNA_DEMO_QUIT_SECONDS="$RECEIVER_SAFETY_SECONDS" \
  GNA_DEMO_QUIT_FILE="$quit_file" \
  GNA_DEMO_TRACE_JSONL="$trace" GNA_DEMO_EVENT_JSONL="$receiver_events" \
  /usr/bin/time -v -o "$OUTPUT_DIR/receiver_time.txt" \
  "${receiver_prefix[@]}" "$GODOT_BIN" --display-driver headless --audio-driver PulseAudio \
  --path "$ROOT_DIR/example_iroh" --scene res://main.tscn \
  >"$OUTPUT_DIR/receiver.log" 2>&1 &
receiver_pid=$!

for _ in $(seq 1 300); do
  [[ -s "$endpoint" ]] && break
  kill -0 "$receiver_pid"
  sleep 0.05
done
[[ -s "$endpoint" ]] || { echo "receiver endpoint unavailable" >&2; exit 1; }

godot_pid=""
for _ in $(seq 1 100); do
  godot_pid="$(pgrep -P "$receiver_pid" | head -1 || true)"
  [[ -n "$godot_pid" ]] && break
  sleep 0.01
done
[[ -n "$godot_pid" ]] || { echo "Godot child process unavailable" >&2; exit 1; }
python3 "$ROOT_DIR/scripts/sample_process_resources.py" "$godot_pid" \
  "$OUTPUT_DIR/receiver_resources.jsonl" &
sampler_pid=$!

loadgen_prefix=(nice -n "$LOADGEN_NICE")
if [[ -n "$LOADGEN_CPUSET" ]]; then
  loadgen_prefix+=(taskset -c "$LOADGEN_CPUSET")
fi
"${loadgen_prefix[@]}" "$ROOT_DIR/target/release/godot_voice_churn" "$endpoint" \
  "$OUTPUT_DIR/loadgen_events.jsonl" "$PEERS" "$ACTIVE" "$PHASE_SECONDS" \
  >"$OUTPUT_DIR/loadgen.json" 2>"$OUTPUT_DIR/loadgen.log" &
loadgen_pid=$!
wait "$loadgen_pid"
loadgen_pid=""
# Let the receiver drain the final connection-close events before asking its
# parent node to exit on the next process frame.
sleep 1
touch "$quit_file"
wait "$receiver_pid"
receiver_pid=""
wait "$sampler_pid" || true
sampler_pid=""
kill -INT "$capture_pid" 2>/dev/null || true
wait "$capture_pid" || true
capture_pid=""

cp "$trace" "$OUTPUT_DIR/receiver_trace.jsonl"

python3 "$ROOT_DIR/scripts/summarize_godot_voice_churn.py" \
  "$OUTPUT_DIR" "$PEERS" "$ACTIVE" "$EXPECTED_SECONDS" >"$OUTPUT_DIR/summary.json"
cat "$OUTPUT_DIR/summary.json"
cat "$OUTPUT_DIR/loadgen.json"

trap - EXIT
pactl unload-module "$module_id"
rm -rf -- "$runtime_dir"

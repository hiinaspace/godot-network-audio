#!/usr/bin/env bash
# Run one Godot spatial receiver against a lightweight Rust Iroh population.
# Usage: run_godot_voice_rust_load.sh [OUTPUT_DIR] [PEERS] [ACTIVE] [SECONDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="$(realpath -m "${1:-$ROOT_DIR/target/godot-rust-load}")"
PEERS="${2:-31}"
ACTIVE="${3:-7}"
MEDIA_SECONDS="${4:-12}"
RECEIVER_SECONDS=$((MEDIA_SECONDS + 3))
GODOT_BIN="${GODOT_BIN:-$(command -v godot || command -v godot4 || true)}"

mkdir -p "$OUTPUT_DIR"
if [[ -f "$HOME/.cargo/env" ]]; then source "$HOME/.cargo/env"; fi
cargo build -p godot_network_audio --features iroh-transport >/dev/null
cargo build --release -p voice-mesh-bench --bin godot_voice_loadgen >/dev/null
bash "$ROOT_DIR/scripts/sync_iroh_example_extensions.sh" >/dev/null

tag="$$"
sink="gna_rust_load_output_${tag}"
module_id=""
receiver_pid=""
capture_pid=""
loadgen_pid=""
cleanup() {
  set +e
  for pid in "$loadgen_pid" "$receiver_pid" "$capture_pid"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
  done
  [[ -n "$module_id" ]] && pactl unload-module "$module_id" 2>/dev/null || true
}
trap cleanup EXIT

module_id="$(pactl load-module module-null-sink sink_name="$sink" rate=48000)"
ffmpeg -hide_banner -loglevel error -nostdin -y -f pulse -i "$sink.monitor" \
  -t "$((RECEIVER_SECONDS + 1))" -ac 2 -ar 48000 "$OUTPUT_DIR/mixed_output.wav" \
  >"$OUTPUT_DIR/output_capture.log" 2>&1 &
capture_pid=$!

endpoint="$OUTPUT_DIR/receiver_endpoint.json"
trace="$OUTPUT_DIR/receiver_trace.jsonl"
env PULSE_SINK="$sink" GNA_IROH_ROLE=receiver GNA_IROH_BIND_ADDR=127.0.0.1:0 \
  GNA_IROH_ENDPOINT_INFO_PATH="$endpoint" GNA_DEMO_OUTPUT_DEVICE="$sink" \
  GNA_DEMO_SPATIALIZE=1 GNA_DEMO_QUIT_SECONDS="$RECEIVER_SECONDS" \
  GNA_DEMO_TRACE_JSONL="$trace" /usr/bin/time -v -o "$OUTPUT_DIR/receiver_time.txt" \
  "$GODOT_BIN" --display-driver headless --audio-driver PulseAudio \
  --path "$ROOT_DIR/example_iroh" --scene res://main.tscn \
  >"$OUTPUT_DIR/receiver.log" 2>&1 &
receiver_pid=$!

for _ in $(seq 1 300); do
  [[ -s "$endpoint" ]] && break
  kill -0 "$receiver_pid"
  sleep 0.05
done
[[ -s "$endpoint" ]] || { echo "receiver endpoint unavailable" >&2; exit 1; }

"$ROOT_DIR/target/release/godot_voice_loadgen" "$endpoint" "$PEERS" "$ACTIVE" "$MEDIA_SECONDS" \
  >"$OUTPUT_DIR/loadgen.json" 2>"$OUTPUT_DIR/loadgen.log" &
loadgen_pid=$!
wait "$loadgen_pid"
loadgen_pid=""
wait "$receiver_pid"
receiver_pid=""
wait "$capture_pid" || true
capture_pid=""

python3 "$ROOT_DIR/scripts/summarize_godot_voice_scale.py" \
  "$OUTPUT_DIR" "$PEERS" "$ACTIVE" "$RECEIVER_SECONDS" >"$OUTPUT_DIR/summary.json"
cat "$OUTPUT_DIR/summary.json"
cat "$OUTPUT_DIR/loadgen.json"

trap - EXIT
pactl unload-module "$module_id"

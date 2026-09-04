#!/usr/bin/env bash
# One Godot receiver with many Iroh peers and independently decoded/spatialized streams.
# Usage: run_godot_voice_scale.sh [OUTPUT_DIR] [CONNECTED_PEERS] [ACTIVE_SPEAKERS] [SECONDS]

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="$(realpath -m "${1:-$ROOT_DIR/target/godot-scale}")"
CONNECTED_PEERS="${2:-8}"
ACTIVE_SPEAKERS="${3:-7}"
RUN_SECONDS="${4:-12}"
GODOT_BIN="${GODOT_BIN:-$(command -v godot || command -v godot4 || true)}"

if [[ -z "$GODOT_BIN" ]]; then
  echo "Godot not found; set GODOT_BIN or install godot/godot4 on PATH" >&2
  exit 1
fi
if (( CONNECTED_PEERS < 1 || ACTIVE_SPEAKERS < 0 || ACTIVE_SPEAKERS > CONNECTED_PEERS )); then
  echo "require CONNECTED_PEERS >= 1 and 0 <= ACTIVE_SPEAKERS <= CONNECTED_PEERS" >&2
  exit 2
fi

mkdir -p "$OUTPUT_DIR/senders"
if [[ -f "$HOME/.cargo/env" ]]; then
  source "$HOME/.cargo/env"
fi
cargo build -p godot_network_audio --features iroh-transport >/dev/null
bash "$ROOT_DIR/scripts/sync_iroh_example_extensions.sh" >/dev/null

RUN_TAG="$$"
OUTPUT_SINK="gna_scale_output_${RUN_TAG}"
OUTPUT_MODULE_ID=""
CAPTURE_PID=""
RECEIVER_PID=""
SENDER_PIDS=()

kill_pid() {
  local pid="${1:-}"
  [[ -z "$pid" ]] && return 0
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  set +e
  for pid in "${SENDER_PIDS[@]}"; do kill_pid "$pid"; done
  kill_pid "$RECEIVER_PID"
  kill_pid "$CAPTURE_PID"
  [[ -n "$OUTPUT_MODULE_ID" ]] && pactl unload-module "$OUTPUT_MODULE_ID" 2>/dev/null || true
}
trap cleanup EXIT

OUTPUT_MODULE_ID="$(pactl load-module module-null-sink sink_name="$OUTPUT_SINK" rate=48000 sink_properties=device.description="$OUTPUT_SINK")"
OUTPUT_WAV="$OUTPUT_DIR/mixed_output.wav"
ffmpeg -hide_banner -loglevel error -nostdin -y -f pulse -i "$OUTPUT_SINK.monitor" \
  -t "$((RUN_SECONDS + 3))" -ac 2 -ar 48000 "$OUTPUT_WAV" \
  >"$OUTPUT_DIR/output_capture.log" 2>&1 &
CAPTURE_PID=$!

ENDPOINT_INFO="$OUTPUT_DIR/receiver_endpoint.json"
RECEIVER_TRACE="$OUTPUT_DIR/receiver_trace.jsonl"
env \
  PULSE_SINK="$OUTPUT_SINK" \
  GNA_IROH_ROLE=receiver \
  GNA_IROH_BIND_ADDR=127.0.0.1:0 \
  GNA_IROH_ENDPOINT_INFO_PATH="$ENDPOINT_INFO" \
  GNA_DEMO_OUTPUT_DEVICE="$OUTPUT_SINK" \
  GNA_DEMO_SPATIALIZE="${GNA_SCALE_SPATIALIZE:-1}" \
  GNA_DEMO_QUIT_SECONDS="$RUN_SECONDS" \
  GNA_DEMO_TRACE_JSONL="$RECEIVER_TRACE" \
  /usr/bin/time -v -o "$OUTPUT_DIR/receiver_time.txt" \
  "$GODOT_BIN" --display-driver headless --audio-driver PulseAudio \
  --path "$ROOT_DIR/example_iroh" --scene res://main.tscn \
  >"$OUTPUT_DIR/receiver.log" 2>&1 &
RECEIVER_PID=$!

for _ in $(seq 1 300); do
  [[ -s "$ENDPOINT_INFO" ]] && break
  kill -0 "$RECEIVER_PID"
  sleep 0.05
done
if [[ ! -s "$ENDPOINT_INFO" ]]; then
  echo "receiver endpoint info was not written" >&2
  exit 1
fi

for peer in $(seq 1 "$CONNECTED_PEERS"); do
  active=0
  if (( peer <= ACTIVE_SPEAKERS )); then active=1; fi
  frequency=$((180 + peer * 23))
  env \
    PULSE_SINK="$OUTPUT_SINK" \
    GNA_IROH_ROLE=sender \
    GNA_IROH_BIND_ADDR=127.0.0.1:0 \
    GNA_IROH_REMOTE_INFO_PATH="$ENDPOINT_INFO" \
    GNA_DEMO_FORCE_SYNTHETIC=1 \
    GNA_DEMO_SEND_AUDIO="$active" \
    GNA_DEMO_SYNTHETIC_FREQUENCY_HZ="$frequency" \
    GNA_DEMO_QUIT_SECONDS="$RUN_SECONDS" \
    "$GODOT_BIN" --display-driver headless --audio-driver PulseAudio \
    --path "$ROOT_DIR/example_iroh" --scene res://main.tscn \
    >"$OUTPUT_DIR/senders/peer-${peer}.log" 2>&1 &
  SENDER_PIDS+=("$!")
done

receiver_status=0
wait "$RECEIVER_PID" || receiver_status=$?
RECEIVER_PID=""
sender_status=0
for pid in "${SENDER_PIDS[@]}"; do
  wait "$pid" || sender_status=$?
done
SENDER_PIDS=()
wait "$CAPTURE_PID" || true
CAPTURE_PID=""

python3 "$ROOT_DIR/scripts/summarize_godot_voice_scale.py" \
  "$OUTPUT_DIR" "$CONNECTED_PEERS" "$ACTIVE_SPEAKERS" "$RUN_SECONDS" \
  >"$OUTPUT_DIR/summary.json"
cat "$OUTPUT_DIR/summary.json"

if (( receiver_status != 0 || sender_status != 0 )); then
  echo "Godot process failed: receiver=$receiver_status sender=$sender_status" >&2
  exit 1
fi

trap - EXIT
pactl unload-module "$OUTPUT_MODULE_ID"

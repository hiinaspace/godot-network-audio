#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 INPUT_WAV OUTPUT_DIR [RUN_SECONDS]" >&2
  exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_WAV="$(realpath "$1")"
OUTPUT_DIR="$(realpath -m "$2")"
RUN_SECONDS="${3:-6}"
GODOT_GRACE_SECONDS="${GNA_DEMO_GODOT_GRACE_SECONDS:-8}"
STARTUP_SECONDS="${GNA_IROH_STARTUP_SECONDS:-4}"
TOTAL_SECONDS="$((RUN_SECONDS + STARTUP_SECONDS))"

mkdir -p "$OUTPUT_DIR"

RUN_TAG="$$"
SENDER_INPUT_SINK="gna_iroh_sender_input_sink_${RUN_TAG}"
SENDER_INPUT_SOURCE="gna_iroh_sender_virtual_mic_${RUN_TAG}"
SENDER_OUTPUT_SINK="gna_iroh_sender_output_sink_${RUN_TAG}"
RECEIVER_OUTPUT_SINK="gna_iroh_receiver_output_sink_${RUN_TAG}"

INPUT_MODULE_ID=""
INPUT_SOURCE_MODULE_ID=""
SENDER_OUTPUT_MODULE_ID=""
RECEIVER_OUTPUT_MODULE_ID=""
PLAYER_PID=""
INPUT_RECORDER_PID=""
OUTPUT_RECORDER_PID=""
SENDER_PID=""
RECEIVER_PID=""
ORIGINAL_DEFAULT_SINK=""
ORIGINAL_DEFAULT_SOURCE=""

kill_pid() {
  local pid="${1:-}"
  if [[ -z "$pid" ]]; then
    return 0
  fi
  kill "$pid" 2>/dev/null || true
  for _ in {1..20}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" 2>/dev/null || true
      return 0
    fi
    sleep 0.1
  done
  kill -9 "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

cleanup() {
  set +e
  kill_pid "$PLAYER_PID"
  kill_pid "$INPUT_RECORDER_PID"
  kill_pid "$OUTPUT_RECORDER_PID"
  kill_pid "$SENDER_PID"
  kill_pid "$RECEIVER_PID"
  [[ -n "$ORIGINAL_DEFAULT_SINK" ]] && pactl set-default-sink "$ORIGINAL_DEFAULT_SINK" 2>/dev/null || true
  [[ -n "$ORIGINAL_DEFAULT_SOURCE" ]] && pactl set-default-source "$ORIGINAL_DEFAULT_SOURCE" 2>/dev/null || true
  # Unload by stored module ID first (fast path).
  [[ -n "$INPUT_SOURCE_MODULE_ID" ]] && pactl unload-module "$INPUT_SOURCE_MODULE_ID" 2>/dev/null || true
  [[ -n "$INPUT_MODULE_ID" ]] && pactl unload-module "$INPUT_MODULE_ID" 2>/dev/null || true
  [[ -n "$SENDER_OUTPUT_MODULE_ID" ]] && pactl unload-module "$SENDER_OUTPUT_MODULE_ID" 2>/dev/null || true
  [[ -n "$RECEIVER_OUTPUT_MODULE_ID" ]] && pactl unload-module "$RECEIVER_OUTPUT_MODULE_ID" 2>/dev/null || true
  # Fallback: scan by name for any modules from this run tag that weren't captured
  # (e.g. if the script crashed before a load-module call completed).
  if [[ -n "$RUN_TAG" ]]; then
    mapfile -t stale_ids < <(pactl list short modules 2>/dev/null \
      | awk -v tag="$RUN_TAG" '$0 ~ tag { print $1 }')
    for mid in "${stale_ids[@]}"; do
      pactl unload-module "$mid" 2>/dev/null || true
    done
  fi
}
trap cleanup EXIT

if [[ -f "$HOME/.cargo/env" ]]; then
  source "$HOME/.cargo/env"
fi
cargo build -p godot_network_audio --features iroh-transport >/dev/null
bash "$ROOT_DIR/scripts/sync_iroh_example_extensions.sh" >/dev/null

ORIGINAL_DEFAULT_SINK="$(pactl info | sed -n 's/^Default Sink: //p' | head -n 1)"
ORIGINAL_DEFAULT_SOURCE="$(pactl info | sed -n 's/^Default Source: //p' | head -n 1)"

INPUT_MODULE_ID="$(pactl load-module module-null-sink sink_name="$SENDER_INPUT_SINK" rate=48000 sink_properties=device.description="$SENDER_INPUT_SINK")"
INPUT_SOURCE_MODULE_ID="$(pactl load-module module-remap-source source_name="$SENDER_INPUT_SOURCE" master="$SENDER_INPUT_SINK.monitor" source_properties=device.description="$SENDER_INPUT_SOURCE")"
SENDER_OUTPUT_MODULE_ID="$(pactl load-module module-null-sink sink_name="$SENDER_OUTPUT_SINK" rate=48000 sink_properties=device.description="$SENDER_OUTPUT_SINK")"
RECEIVER_OUTPUT_MODULE_ID="$(pactl load-module module-null-sink sink_name="$RECEIVER_OUTPUT_SINK" rate=48000 sink_properties=device.description="$RECEIVER_OUTPUT_SINK")"
pactl set-default-source "$SENDER_INPUT_SOURCE"
pactl set-default-sink "$RECEIVER_OUTPUT_SINK"

INPUT_CAPTURE_WAV="$OUTPUT_DIR/input_virtual_mic.wav"
OUTPUT_CAPTURE_WAV="$OUTPUT_DIR/output_receiver_sink.wav"
SENDER_LOG="$OUTPUT_DIR/sender.log"
RECEIVER_LOG="$OUTPUT_DIR/receiver.log"
SENDER_TRACE="$OUTPUT_DIR/sender_trace.jsonl"
RECEIVER_TRACE="$OUTPUT_DIR/receiver_trace.jsonl"
STATS_PNG="$OUTPUT_DIR/demo_stats.png"
SPECTROGRAM_PNG="$OUTPUT_DIR/demo_io_spectrograms.png"
RECEIVER_INFO_JSON="$OUTPUT_DIR/receiver_endpoint.json"

ffmpeg -hide_banner -loglevel error -nostdin \
  -stream_loop -1 -re -i "$INPUT_WAV" \
  -t "$((TOTAL_SECONDS + GODOT_GRACE_SECONDS + 2))" \
  -ac 2 -ar 48000 -f pulse -device "$SENDER_INPUT_SINK" "$SENDER_INPUT_SINK" \
  >"$OUTPUT_DIR/input_player.log" 2>&1 &
PLAYER_PID=$!

ffmpeg -hide_banner -loglevel error -nostdin -y \
  -f pulse -i "$SENDER_INPUT_SINK.monitor" -t "$TOTAL_SECONDS" \
  -ac 2 -ar 48000 "$INPUT_CAPTURE_WAV" \
  >"$OUTPUT_DIR/input_capture.log" 2>&1 &
INPUT_RECORDER_PID=$!

ffmpeg -hide_banner -loglevel error -nostdin -y \
  -f pulse -i "$RECEIVER_OUTPUT_SINK.monitor" -t "$TOTAL_SECONDS" \
  -ac 2 -ar 48000 "$OUTPUT_CAPTURE_WAV" \
  >"$OUTPUT_DIR/output_capture.log" 2>&1 &
OUTPUT_RECORDER_PID=$!

if [[ -n "${GODOT_BIN:-}" ]]; then
  :
elif command -v fgvm >/dev/null 2>&1; then
  GODOT_BIN="$(fgvm which | tail -n 1 | sed 's/\x1b\[[0-9;]*m//g' | awk '{print $NF}')"
elif command -v godot >/dev/null 2>&1; then
  GODOT_BIN="$(command -v godot)"
elif command -v godot4 >/dev/null 2>&1; then
  GODOT_BIN="$(command -v godot4)"
else
  echo "Godot not found; set GODOT_BIN or install godot/godot4 on PATH" >&2
  exit 1
fi

PULSE_SINK="$RECEIVER_OUTPUT_SINK" \
PULSE_SOURCE="$SENDER_INPUT_SOURCE" \
GNA_IROH_ROLE=receiver \
GNA_IROH_ENDPOINT_INFO_PATH="$RECEIVER_INFO_JSON" \
GNA_IROH_BIND_ADDR="${GNA_IROH_BIND_ADDR_RECEIVER:-}" \
GNA_DEMO_OUTPUT_DEVICE="$RECEIVER_OUTPUT_SINK" \
GNA_DEMO_QUIT_SECONDS="${GNA_DEMO_QUIT_SECONDS:-$TOTAL_SECONDS}" \
GNA_DEMO_TRACE_JSONL="$RECEIVER_TRACE" \
GNA_DEMO_MAX_FPS="${GNA_IROH_RECEIVER_MAX_FPS:-5}" \
  "$GODOT_BIN" --display-driver headless --audio-driver PulseAudio \
  --path "$ROOT_DIR/example_iroh" --scene res://main.tscn --verbose \
  >"$RECEIVER_LOG" 2>&1 &
RECEIVER_PID=$!

for _ in {1..100}; do
  if [[ -s "$RECEIVER_INFO_JSON" ]]; then
    break
  fi
  sleep 0.1
done

if [[ ! -s "$RECEIVER_INFO_JSON" ]]; then
  echo "receiver endpoint info was not written" >&2
  exit 1
fi

PULSE_SINK="$SENDER_OUTPUT_SINK" \
PULSE_SOURCE="$SENDER_INPUT_SOURCE" \
GNA_IROH_ROLE=sender \
GNA_IROH_REMOTE_INFO_PATH="$RECEIVER_INFO_JSON" \
GNA_IROH_BIND_ADDR="${GNA_IROH_BIND_ADDR_SENDER:-}" \
GNA_DEMO_INPUT_DEVICE="$SENDER_INPUT_SOURCE" \
GNA_DEMO_OUTPUT_DEVICE="$SENDER_OUTPUT_SINK" \
GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK="${GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK:-0}" \
GNA_DEMO_QUIT_SECONDS="${GNA_DEMO_QUIT_SECONDS:-$TOTAL_SECONDS}" \
GNA_DEMO_TRACE_JSONL="$SENDER_TRACE" \
  "$GODOT_BIN" --display-driver headless --audio-driver PulseAudio \
  --path "$ROOT_DIR/example_iroh" --scene res://main.tscn --verbose \
  >"$SENDER_LOG" 2>&1 &
SENDER_PID=$!

sleep "$TOTAL_SECONDS"

kill_pid "$SENDER_PID"; SENDER_PID=""
kill_pid "$RECEIVER_PID"; RECEIVER_PID=""
kill_pid "$INPUT_RECORDER_PID"; INPUT_RECORDER_PID=""
kill_pid "$OUTPUT_RECORDER_PID"; OUTPUT_RECORDER_PID=""
kill_pid "$PLAYER_PID"; PLAYER_PID=""

uv run "$ROOT_DIR/scripts/plot_demo_stats.py" \
  --sender "$SENDER_TRACE" --receiver "$RECEIVER_TRACE" "$STATS_PNG" >/dev/null

uv run "$ROOT_DIR/scripts/plot_demo_io_spectrograms.py" "$INPUT_CAPTURE_WAV" "$OUTPUT_CAPTURE_WAV" "$SPECTROGRAM_PNG" >/dev/null

printf '%s\n' \
  "$INPUT_CAPTURE_WAV" \
  "$OUTPUT_CAPTURE_WAV" \
  "$SENDER_LOG" \
  "$RECEIVER_LOG" \
  "$SENDER_TRACE" \
  "$RECEIVER_TRACE" \
  "$STATS_PNG" \
  "$SPECTROGRAM_PNG"

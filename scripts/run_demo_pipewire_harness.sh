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

if [[ ! -f "$INPUT_WAV" ]]; then
  echo "input wav not found: $INPUT_WAV" >&2
  exit 1
fi

mkdir -p "$OUTPUT_DIR"

RUN_TAG="$$"
INPUT_SINK="gna_demo_input_sink_${RUN_TAG}"
INPUT_SOURCE="gna_demo_virtual_mic_${RUN_TAG}"
OUTPUT_SINK="gna_demo_output_sink_${RUN_TAG}"
INPUT_MODULE_ID=""
INPUT_SOURCE_MODULE_ID=""
OUTPUT_MODULE_ID=""
PLAYER_PID=""
RECORDER_PID=""
INPUT_RECORDER_PID=""
GODOT_PID=""
ORIGINAL_DEFAULT_SINK=""
ORIGINAL_DEFAULT_SOURCE=""

kill_pid() {
  local pid="${1:-}"
  if [[ -z "$pid" ]]; then
    return 0
  fi
  kill "$pid" 2>/dev/null || true
  for _ in {1..10}; do
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
  kill_pid "$RECORDER_PID"
  kill_pid "$INPUT_RECORDER_PID"
  kill_pid "$GODOT_PID"
  if [[ -n "$ORIGINAL_DEFAULT_SINK" ]]; then
    pactl set-default-sink "$ORIGINAL_DEFAULT_SINK" 2>/dev/null || true
  fi
  if [[ -n "$ORIGINAL_DEFAULT_SOURCE" ]]; then
    pactl set-default-source "$ORIGINAL_DEFAULT_SOURCE" 2>/dev/null || true
  fi
  if [[ -n "$INPUT_SOURCE_MODULE_ID" ]]; then
    pactl unload-module "$INPUT_SOURCE_MODULE_ID" 2>/dev/null || true
  fi
  if [[ -n "$INPUT_MODULE_ID" ]]; then
    pactl unload-module "$INPUT_MODULE_ID" 2>/dev/null || true
  fi
  if [[ -n "$OUTPUT_MODULE_ID" ]]; then
    pactl unload-module "$OUTPUT_MODULE_ID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

source "$HOME/.cargo/env"
cargo build -p godot_network_audio >/dev/null
bash "$ROOT_DIR/scripts/sync_demo_extension.sh" >/dev/null

ORIGINAL_DEFAULT_SINK="$(pactl info | sed -n 's/^Default Sink: //p' | head -n 1)"
ORIGINAL_DEFAULT_SOURCE="$(pactl info | sed -n 's/^Default Source: //p' | head -n 1)"

INPUT_MODULE_ID="$(pactl load-module module-null-sink sink_name="$INPUT_SINK" rate=48000 sink_properties=device.description="$INPUT_SINK")"
INPUT_SOURCE_MODULE_ID="$(pactl load-module module-remap-source source_name="$INPUT_SOURCE" master="$INPUT_SINK.monitor" source_properties=device.description="$INPUT_SOURCE")"
OUTPUT_MODULE_ID="$(pactl load-module module-null-sink sink_name="$OUTPUT_SINK" rate=48000 sink_properties=device.description="$OUTPUT_SINK")"
pactl set-default-source "$INPUT_SOURCE"
pactl set-default-sink "$OUTPUT_SINK"

INPUT_CAPTURE_WAV="$OUTPUT_DIR/input_virtual_mic.wav"
OUTPUT_CAPTURE_WAV="$OUTPUT_DIR/output_godot_sink.wav"
LOG_PATH="$OUTPUT_DIR/godot_demo.log"
TRACE_JSONL_PATH="$OUTPUT_DIR/demo_trace.jsonl"
STATS_PNG_PATH="$OUTPUT_DIR/demo_stats.png"
IO_SPECTROGRAM_PNG_PATH="$OUTPUT_DIR/demo_io_spectrograms.png"

ffmpeg -hide_banner -loglevel error -nostdin \
  -stream_loop -1 -re -i "$INPUT_WAV" \
  -t "$((RUN_SECONDS + GODOT_GRACE_SECONDS + 2))" \
  -ac 2 -ar 48000 -f pulse -device "$INPUT_SINK" "$INPUT_SINK" \
  >"$OUTPUT_DIR/input_player.log" 2>&1 &
PLAYER_PID=$!

ffmpeg -hide_banner -loglevel error -nostdin -y \
  -f pulse -i "$INPUT_SINK.monitor" -t "$RUN_SECONDS" \
  -ac 2 -ar 48000 "$INPUT_CAPTURE_WAV" \
  >"$OUTPUT_DIR/input_capture.log" 2>&1 &
INPUT_RECORDER_PID=$!

ffmpeg -hide_banner -loglevel error -nostdin -y \
  -f pulse -i "$OUTPUT_SINK.monitor" -t "$RUN_SECONDS" \
  -ac 2 -ar 48000 "$OUTPUT_CAPTURE_WAV" \
  >"$OUTPUT_DIR/output_capture.log" 2>&1 &
RECORDER_PID=$!

sleep 0.5

if command -v fgvm >/dev/null 2>&1; then
  GODOT_BIN="$(fgvm which | tail -n 1)"
elif [[ -x "$HOME/bin/fgvm" ]]; then
  GODOT_BIN="$("$HOME/bin/fgvm" which | tail -n 1)"
else
  GODOT_BIN="$HOME/fgvm/4.6.1-stable-standard/Godot_v4.6.1-stable_linux.x86_64"
fi

PULSE_SOURCE="$INPUT_SOURCE" \
PULSE_SINK="$OUTPUT_SINK" \
GNA_DEMO_INPUT_DEVICE="$INPUT_SOURCE" \
GNA_DEMO_OUTPUT_DEVICE="$OUTPUT_SINK" \
GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK="${GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK:-0}" \
GNA_DEMO_QUIT_SECONDS="${GNA_DEMO_QUIT_SECONDS:-$RUN_SECONDS}" \
GNA_DEMO_TRACE_JSONL="$TRACE_JSONL_PATH" \
  "$GODOT_BIN" --path "$ROOT_DIR/example" --scene res://main.tscn --verbose \
  >"$LOG_PATH" 2>&1 &
GODOT_PID=$!

sleep "$RUN_SECONDS"

GODOT_STATUS=0
if kill -0 "$GODOT_PID" 2>/dev/null; then
  kill "$GODOT_PID" 2>/dev/null || true
  for _ in $(seq 1 $((GODOT_GRACE_SECONDS * 10))); do
    if ! kill -0 "$GODOT_PID" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
  if kill -0 "$GODOT_PID" 2>/dev/null; then
    kill -9 "$GODOT_PID" 2>/dev/null || true
  fi
fi

set +e
wait "$GODOT_PID"
GODOT_STATUS=$?
set -e
GODOT_PID=""

if [[ "$GODOT_STATUS" -ne 0 && "$GODOT_STATUS" -ne 143 && "$GODOT_STATUS" -ne 137 ]]; then
  echo "godot exited with status $GODOT_STATUS" >&2
  exit "$GODOT_STATUS"
fi

sleep 0.5
kill_pid "$RECORDER_PID"
kill_pid "$INPUT_RECORDER_PID"

wait "$RECORDER_PID" || true
RECORDER_PID=""

wait "$INPUT_RECORDER_PID" || true
INPUT_RECORDER_PID=""

kill_pid "$PLAYER_PID"
PLAYER_PID=""

if [[ -f "$TRACE_JSONL_PATH" ]]; then
  uv run "$ROOT_DIR/scripts/plot_demo_stats.py" "$TRACE_JSONL_PATH" "$STATS_PNG_PATH" >/dev/null
elif [[ -f "$LOG_PATH" ]]; then
  uv run "$ROOT_DIR/scripts/plot_demo_stats.py" "$LOG_PATH" "$STATS_PNG_PATH" >/dev/null
fi

if [[ -f "$INPUT_CAPTURE_WAV" && -f "$OUTPUT_CAPTURE_WAV" ]]; then
  uv run "$ROOT_DIR/scripts/plot_demo_io_spectrograms.py" \
    "$INPUT_CAPTURE_WAV" \
    "$OUTPUT_CAPTURE_WAV" \
    "$IO_SPECTROGRAM_PNG_PATH" \
    >/dev/null
fi

printf '%s\n' \
  "$INPUT_CAPTURE_WAV" \
  "$OUTPUT_CAPTURE_WAV" \
  "$LOG_PATH" \
  "$TRACE_JSONL_PATH" \
  "$STATS_PNG_PATH" \
  "$IO_SPECTROGRAM_PNG_PATH"

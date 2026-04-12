#!/usr/bin/env bash
set -euo pipefail

pkill -f 'scripts/run_demo_pipewire_harness.sh' 2>/dev/null || true
pkill -f 'ffmpeg -hide_banner -loglevel error -nostdin -stream_loop -1 -re -i .* -f pulse -device gna_demo_input_sink_' 2>/dev/null || true

mapfile -t module_ids < <(pactl list short modules | awk '/gna_demo_/ { print $1 }')
for module_id in "${module_ids[@]}"; do
  pactl unload-module "$module_id" 2>/dev/null || true
done

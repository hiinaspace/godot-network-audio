#!/usr/bin/env bash
# Sweep iroh voice quality across a range of tc netem network impairment profiles.
#
# Usage:
#   run_iroh_netem_sweep.sh INPUT_WAV OUTPUT_DIR [RUN_SECONDS]
#
# Each profile runs the two-process iroh demo over a veth pair with netem shaping
# on both ends, captures JSONL traces, extracts a summary row, and writes a CSV
# plus aggregate plot.
#
# Prerequisites:
#   - sudo access for ip/tc commands
#   - PipeWire/PulseAudio running (needed by the inner harness)
#   - GNA_IROH_RECEIVER_MAX_FPS not set (defaults to 5 inside the harness)
#
# Environment overrides:
#   GNA_DEMO_GODOT_GRACE_SECONDS  — extra seconds Godot gets after RUN_SECONDS (default 8)
#   GNA_IROH_STARTUP_SECONDS      — startup wait before recording begins (default 4)

set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 INPUT_WAV OUTPUT_DIR [RUN_SECONDS]" >&2
  exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_WAV="$(realpath "$1")"
OUTPUT_DIR="$(realpath -m "$2")"
RUN_SECONDS="${3:-15}"

TX_IF="veth-gna-tx"
RX_IF="veth-gna-rx"
TX_ADDR="10.99.0.1"
RX_ADDR="10.99.0.2"

SWEEP_CSV="$OUTPUT_DIR/sweep_results.csv"
SWEEP_PNG="$OUTPUT_DIR/sweep.png"

mkdir -p "$OUTPUT_DIR"

source "$HOME/.cargo/env"
echo "Building extension..."
cargo build -p godot_network_audio --features iroh-transport >/dev/null
bash "$ROOT_DIR/scripts/sync_iroh_example_extensions.sh" >/dev/null

echo "Setting up veth pair..."
bash "$ROOT_DIR/scripts/setup_gna_veth.sh" up

remove_netem() {
  sudo tc qdisc del dev "$TX_IF" root 2>/dev/null || true
  sudo tc qdisc del dev "$RX_IF" root 2>/dev/null || true
}

apply_netem() {
  local delay_ms="$1" jitter_ms="$2" loss_spec="$3" dup_pct="$4"
  # loss_spec is either "X%" (random loss) or "gemodel_P_R_1-H[_1-K]" (burst).
  # Underscores in gemodel specs are replaced with spaces for tc.
  local loss_arg="${loss_spec//_/ }"
  remove_netem
  for iface in "$TX_IF" "$RX_IF"; do
    local args=()
    # distribution normal requires non-zero delay+jitter.
    if [[ "$delay_ms" != "0" || "$jitter_ms" != "0" ]]; then
      args+=(delay "${delay_ms}ms" "${jitter_ms}ms" distribution normal)
    fi
    # tc netem rejects loss/duplicate when the value is zero — omit them.
    if [[ "$loss_spec" != "0%" && "$loss_spec" != "0" ]]; then
      args+=(loss $loss_arg)
    fi
    if [[ "$dup_pct" != "0%" && "$dup_pct" != "0" ]]; then
      args+=(duplicate "$dup_pct")
    fi
    sudo tc qdisc add dev "$iface" root netem "${args[@]}"
  done
}

# Extract sweep summary from receiver + sender trace JSONLs.
# Writes one CSV row to stdout.
# emitted_packets (sender) vs enqueued_packets (receiver) reveals QUIC-level drops.
extract_summary_row() {
  local profile="$1" run_sec="$2" receiver_trace="$3" sender_trace="${4:-}"
  uv run --quiet python3 - "$profile" "$run_sec" "$receiver_trace" "$sender_trace" <<'PYEOF'
import json, sys

profile      = sys.argv[1]
run_sec      = float(sys.argv[2])
rx_path      = sys.argv[3]
tx_path      = sys.argv[4] if len(sys.argv) > 4 else ""

def load_jsonl(path):
    rows = []
    if not path:
        return rows
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return rows

rx_rows = load_jsonl(rx_path)
tx_rows = load_jsonl(tx_path)

if not rx_rows:
    print(f"{profile},0,0,0,0,0,0", flush=True)
    sys.exit(0)

Q14 = 16384.0

def last_field(rows, side, key, default=0):
    for row in reversed(rows):
        v = row.get(side, {}).get(key)
        if v is not None:
            return v
    return default

concealed_samples = last_field(rx_rows, "receiver", "concealed_samples", 0)
expand_rate_q14   = last_field(rx_rows, "receiver", "expand_rate", 0)
dropped_packets   = last_field(rx_rows, "receiver", "dropped_packets", 0)
enqueued_packets  = last_field(rx_rows, "receiver", "enqueued_packets", 0)
max_enqueue_ms    = max(
    (r.get("receiver", {}).get("max_enqueue_interval_ms", 0.0) or 0.0) for r in rx_rows
)

# emitted_packets: prefer sender trace, fall back to receiver-side sender stats
emitted_packets = last_field(tx_rows, "sender", "emitted_packets", 0) if tx_rows else 0
if emitted_packets == 0:
    emitted_packets = last_field(rx_rows, "sender", "emitted_packets", 0)

concealment_pct = 100.0 * (concealed_samples / 48_000.0) / max(run_sec, 1.0)
expand_rate_pct = 100.0 * expand_rate_q14 / Q14

print(f"{profile},{concealment_pct:.3f},{expand_rate_pct:.3f},{max_enqueue_ms:.1f},{dropped_packets},{enqueued_packets},{emitted_packets}", flush=True)
PYEOF
}

# Profile table: name delay_ms jitter_ms loss_spec dup_pct
#
# loss_spec is either "X%" for uniform random loss, or
# "gemodel_P_R_1-H[_1-K]" for Gilbert-Elliott burst loss (underscores → spaces in tc).
#
# burst_short: WiFi-like microwave interference.
#   p=3% (enter burst), r=50% (exit burst) → avg burst = 2 pkts = 40 ms.
#   100% loss in burst, 0% in good state.  Expected avg loss ≈ 5.7%.
#
# burst_long: mobile handoff / congestion event.
#   p=0.5% (enter burst), r=5% (exit burst) → avg burst = 20 pkts = 400 ms.
#   100% loss in burst, 0% in good state.  Expected avg loss ≈ 9%.
#   A 400 ms blackout exceeds max_delay_ms=120 ms so NetEq must conceal heavily.
PROFILES=(
  "baseline     0    0    0%                          0%"
  "lan          2    1    0.05%                       0%"
  "wan_good    30    5    0.10%                       0%"
  "wan_mid     60   10    0.50%                       0%"
  "wan_poor   100   20    1%                          0%"
  "mobile      80   30    2%                          0.1%"
  "burst_short 20    5    gemodel_3%_50%_100%         0%"
  "burst_long  60   10    gemodel_0.5%_5%_100%        0%"
)

echo "name,concealment_pct,expand_rate_pct,max_enqueue_interval_ms,dropped_packets,enqueued_packets,emitted_packets" \
  > "$SWEEP_CSV"

for profile_spec in "${PROFILES[@]}"; do
  read -r name delay_ms jitter_ms loss_spec dup_pct <<<"$profile_spec"
  PROFILE_DIR="$OUTPUT_DIR/profile_${name}"
  mkdir -p "$PROFILE_DIR"

  echo ""
  echo "--- Profile: $name (delay=${delay_ms}ms jitter=${jitter_ms}ms loss=${loss_spec} dup=${dup_pct}) ---"
  apply_netem "$delay_ms" "$jitter_ms" "$loss_spec" "$dup_pct"

  GNA_IROH_BIND_ADDR_SENDER="${TX_ADDR}:0" \
  GNA_IROH_BIND_ADDR_RECEIVER="${RX_ADDR}:0" \
  GNA_DEMO_MIN_DELAY_MS=80 \
    bash "$ROOT_DIR/scripts/run_iroh_demo_pipewire_harness.sh" \
      "$INPUT_WAV" "$PROFILE_DIR" "$RUN_SECONDS" \
    || echo "  (harness exited non-zero — check $PROFILE_DIR/receiver.log)"

  remove_netem

  RECEIVER_TRACE="$PROFILE_DIR/receiver_trace.jsonl"
  SENDER_TRACE="$PROFILE_DIR/sender_trace.jsonl"
  if [[ -f "$RECEIVER_TRACE" ]]; then
    ROW=$(extract_summary_row "$name" "$RUN_SECONDS" "$RECEIVER_TRACE" "$SENDER_TRACE")
    echo "  summary: $ROW"
    echo "$ROW" >> "$SWEEP_CSV"
  else
    echo "  WARNING: no receiver trace at $RECEIVER_TRACE" >&2
    echo "${name},,,,,,," >> "$SWEEP_CSV"
  fi
done

echo ""
echo "Sweep complete. Generating plot..."
uv run "$ROOT_DIR/scripts/plot_netem_sweep.py" "$SWEEP_CSV" "$SWEEP_PNG"

printf '\nOutput files:\n'
printf '  %s\n' "$SWEEP_CSV" "$SWEEP_PNG"
echo ""
echo "Per-profile traces and plots:"
for profile_spec in "${PROFILES[@]}"; do
  read -r name _ _ _ _ <<<"$profile_spec"
  PROFILE_DIR="$OUTPUT_DIR/profile_${name}"
  printf '  %s/\n' "$PROFILE_DIR"
done

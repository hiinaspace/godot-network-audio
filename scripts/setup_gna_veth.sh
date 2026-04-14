#!/usr/bin/env bash
# Create or destroy the veth pair used for netem impairment testing.
#
# Usage:
#   setup_gna_veth.sh up    — create veth-gna-tx (10.99.0.1/24) + veth-gna-rx (10.99.0.2/24)
#   setup_gna_veth.sh down  — delete the pair (idempotent)
#
# Requires sudo for ip/link commands.
# The pair is created in the host network namespace — no namespaces needed.

set -euo pipefail

TX_IF="veth-gna-tx"
RX_IF="veth-gna-rx"
TX_ADDR="10.99.0.1/24"
RX_ADDR="10.99.0.2/24"

case "${1:-}" in
  up)
    if ip link show "$TX_IF" &>/dev/null; then
      echo "veth pair already exists ($TX_IF / $RX_IF)" >&2
      exit 0
    fi
    sudo ip link add "$TX_IF" type veth peer name "$RX_IF"
    sudo ip addr add "$TX_ADDR" dev "$TX_IF"
    sudo ip addr add "$RX_ADDR" dev "$RX_IF"
    sudo ip link set "$TX_IF" up
    sudo ip link set "$RX_IF" up
    echo "veth pair up: $TX_IF ($TX_ADDR) <-> $RX_IF ($RX_ADDR)"
    ;;
  down)
    if ip link show "$TX_IF" &>/dev/null; then
      sudo ip link del "$TX_IF"
      echo "veth pair deleted"
    else
      echo "veth pair not present, nothing to do" >&2
    fi
    ;;
  *)
    echo "usage: $0 [up|down]" >&2
    exit 2
    ;;
esac

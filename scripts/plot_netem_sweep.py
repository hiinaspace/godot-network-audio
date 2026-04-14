#!/usr/bin/env -S uv run
# /// script
# dependencies = [
#   "matplotlib>=3.8",
# ]
# ///
"""
Plot aggregate results from a netem sweep CSV.

Usage:
  plot_netem_sweep.py SWEEP_CSV [OUTPUT_PNG]

SWEEP_CSV is produced by run_iroh_netem_sweep.sh.  OUTPUT_PNG defaults to
sweep.png next to the CSV.

Columns expected in the CSV (written by the sweep script):
  name, concealment_pct, expand_rate_pct, max_enqueue_interval_ms,
  dropped_packets, enqueued_packets
"""

from __future__ import annotations

import csv
import sys
from pathlib import Path

import matplotlib.pyplot as plt
import matplotlib.ticker as ticker

MIN_DELAY_MS = 80  # reference floor for Panel 3


def load_csv(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        reader = csv.DictReader(f)
        for row in reader:
            rows.append(row)
    return rows


def safe_float(v: str, default: float = 0.0) -> float:
    try:
        return float(v)
    except (ValueError, TypeError):
        return default


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2

    csv_path = Path(sys.argv[1])
    out_png = Path(sys.argv[2]) if len(sys.argv) > 2 else csv_path.with_name("sweep.png")

    rows = load_csv(csv_path)
    if not rows:
        print(f"no data in {csv_path}", file=sys.stderr)
        return 1

    names = [r["name"] for r in rows]
    x = list(range(len(names)))

    concealment_pct   = [safe_float(r.get("concealment_pct", ""))   for r in rows]
    expand_rate_pct   = [safe_float(r.get("expand_rate_pct", ""))   for r in rows]
    max_enqueue_ms    = [safe_float(r.get("max_enqueue_interval_ms", "")) for r in rows]
    dropped_packets   = [safe_float(r.get("dropped_packets", ""))   for r in rows]
    enqueued_packets  = [safe_float(r.get("enqueued_packets", ""))  for r in rows]
    emitted_packets   = [safe_float(r.get("emitted_packets", ""))   for r in rows]

    fig, axes = plt.subplots(4, 1, figsize=(10, 12), constrained_layout=True)
    bar_kw = dict(color="steelblue", edgecolor="white", linewidth=0.5)

    def _cap_and_annotate(ax: "plt.Axes", values: list[float], cap: float) -> list[float]:
        """Return values clamped to cap; annotate bars that were clipped."""
        capped = [min(v, cap) for v in values]
        for xi, (raw, clipped) in enumerate(zip(values, capped)):
            if raw > cap:
                ax.text(xi, cap * 0.97, f"  {raw:.0f}", va="top", ha="center",
                        fontsize=7, color="crimson", rotation=90)
        return capped

    # Panel 0: concealment %
    ax = axes[0]
    # Cap at 5× the second-largest value to avoid a single outlier crushing the axis.
    sorted_c = sorted(concealment_pct, reverse=True)
    conc_cap = max(sorted_c[1] * 5, 2.0) if len(sorted_c) > 1 else max(max(concealment_pct, default=0) * 1.2, 2.0)
    capped_conc = _cap_and_annotate(ax, concealment_pct, conc_cap)
    ax.bar(x, capped_conc, **bar_kw)
    ax.set_title("Concealment % of run duration")
    ax.set_ylabel("concealment %")
    ax.set_xticks(x)
    ax.set_xticklabels(names, rotation=15, ha="right")
    ax.set_ylim(0, conc_cap * 1.1)
    ax.yaxis.set_major_formatter(ticker.FormatStrFormatter("%.2f%%"))
    ax.axhline(1.0, color="darkorange", lw=1, ls="--", label="1% threshold")
    ax.legend(fontsize=8)

    # Panel 1: final expand_rate %
    ax = axes[1]
    sorted_e = sorted(expand_rate_pct, reverse=True)
    exp_cap = max(sorted_e[1] * 5, 1.0) if len(sorted_e) > 1 else max(max(expand_rate_pct, default=0) * 1.2, 1.0)
    capped_exp = _cap_and_annotate(ax, expand_rate_pct, exp_cap)
    ax.bar(x, capped_exp, color="tomato", edgecolor="white", linewidth=0.5)
    ax.set_title("NetEq expand_rate % (lifetime PLC fraction)")
    ax.set_ylabel("expand_rate %")
    ax.set_xticks(x)
    ax.set_xticklabels(names, rotation=15, ha="right")
    ax.set_ylim(0, exp_cap * 1.1)
    ax.yaxis.set_major_formatter(ticker.FormatStrFormatter("%.2f%%"))
    ax.axhline(0.1, color="darkorange", lw=1, ls="--", label="0.1% threshold")
    ax.legend(fontsize=8)

    # Panel 2: max_enqueue_interval_ms with min_delay reference
    # Cap at 3× the second-largest value so a startup-gap outlier doesn't crush the axis.
    ax = axes[2]
    sorted_m = sorted(max_enqueue_ms, reverse=True)
    enq_cap = max(sorted_m[1] * 3, MIN_DELAY_MS * 2) if len(sorted_m) > 1 else max(max(max_enqueue_ms, default=0) * 1.2, MIN_DELAY_MS * 2)
    capped_enq = _cap_and_annotate(ax, max_enqueue_ms, enq_cap)
    ax.bar(x, capped_enq, color="seagreen", edgecolor="white", linewidth=0.5)
    ax.set_title("Worst-case packet arrival gap (max_enqueue_interval_ms)")
    ax.set_ylabel("ms")
    ax.set_xticks(x)
    ax.set_xticklabels(names, rotation=15, ha="right")
    ax.set_ylim(0, enq_cap * 1.1)
    ax.axhline(MIN_DELAY_MS, color="navy", lw=1.5, ls="--",
               label=f"min_delay_ms = {MIN_DELAY_MS} ms")
    ax.axhline(20, color="gray", lw=1, ls=":", label="nominal packet interval (20 ms)")
    ax.legend(fontsize=8)

    # Panel 3: packet accounting — emitted (sender) vs enqueued (receiver) vs dropped (queue overflow)
    # Gap between emitted and enqueued reveals QUIC-level packet loss.
    ax = axes[3]
    w = 0.28
    ax.bar([xi - w for xi in x], emitted_packets,  width=w, color="steelblue",  label="emitted (sender QUIC out)")
    ax.bar([xi      for xi in x], enqueued_packets, width=w, color="seagreen",   label="enqueued (receiver NetEq in)")
    ax.bar([xi + w for xi in x], dropped_packets,  width=w, color="crimson",    label="dropped (NetEq queue overflow)")
    ax.set_title("Packet accounting: emitted vs enqueued vs dropped")
    ax.set_ylabel("packets")
    ax.set_xticks(x)
    ax.set_xticklabels(names, rotation=15, ha="right")
    ax.set_ylim(0)
    ax.legend(fontsize=8)
    ax.text(0.01, 0.97, "emitted − enqueued = QUIC-level loss (netem + congestion)",
            transform=ax.transAxes, fontsize=7, va="top", color="dimgray")

    fig.suptitle(
        f"netem sweep — {csv_path.name}",
        fontsize=11,
    )

    out_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_png, dpi=150)
    print(out_png)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

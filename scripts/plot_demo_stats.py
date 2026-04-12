#!/usr/bin/env -S uv run
# /// script
# dependencies = [
#   "matplotlib>=3.8",
# ]
# ///
"""
Plot per-second sender/receiver stats from a godot_demo.log file.

Usage:
  plot_demo_stats.py LOGFILE [OUTPUT_PNG]

If OUTPUT_PNG is omitted the PNG is written next to the log file as
demo_stats.png.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import matplotlib.pyplot as plt
import matplotlib.ticker as ticker

STATS_RE = re.compile(
    r"demo: stats receiver=(\{.*?\}) sender=(\{.*?\}) input_mode=(\S+)"
)

Q14_SCALE = 16384.0  # neteq expand_rate / accelerate_rate are Q14 fractions


def parse_log(path: Path) -> list[dict]:
    rows = []
    for line in path.read_text().splitlines():
        m = STATS_RE.search(line)
        if not m:
            continue
        r = json.loads(m.group(1))
        s = json.loads(m.group(2))
        rows.append({"r": r, "s": s, "mode": m.group(3)})
    return rows


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print("usage: plot_demo_stats.py LOGFILE [OUTPUT_PNG]", file=sys.stderr)
        return 2

    log_path = Path(sys.argv[1])
    out_png = (
        Path(sys.argv[2])
        if len(sys.argv) == 3
        else log_path.parent / "demo_stats.png"
    )

    rows = parse_log(log_path)
    if not rows:
        print(f"no 'demo: stats' lines found in {log_path}", file=sys.stderr)
        return 1

    t = list(range(1, len(rows) + 1))

    # --- sender series ---
    sent_cumul = [row["s"].get("packets_sent", 0) for row in rows]
    sent_delta = [sent_cumul[0]] + [
        max(0, sent_cumul[i] - sent_cumul[i - 1]) for i in range(1, len(sent_cumul))
    ]
    max_pkt_ms = [row["s"].get("max_packet_interval_ms", 0.0) for row in rows]
    # running avg in ms (cumulative stat, not per-second)
    avg_pkt_ms = [row["s"].get("avg_packet_interval_ms", 0.0) for row in rows]

    captured_cumul = [row["s"].get("captured_input_frames", 0) for row in rows]
    captured_delta = [captured_cumul[0]] + [
        max(0, captured_cumul[i] - captured_cumul[i - 1])
        for i in range(1, len(captured_cumul))
    ]
    expected_frames_per_sec = 48_000
    capture_pct = [100.0 * d / expected_frames_per_sec for d in captured_delta]

    # --- receiver series ---
    conc_cumul = [row["r"].get("concealed_samples", 0) for row in rows]
    conc_delta_ms = [
        conc_cumul[0] / 48.0  # samples → ms
    ] + [
        max(0, conc_cumul[i] - conc_cumul[i - 1]) / 48.0
        for i in range(1, len(conc_cumul))
    ]

    # expand_rate / accelerate_rate are Q14 cumulative fractions (0–16384 = 0–100%)
    expand_pct = [100.0 * row["r"].get("expand_rate", 0) / Q14_SCALE for row in rows]
    accel_pct = [
        100.0 * row["r"].get("accelerate_rate", 0) / Q14_SCALE for row in rows
    ]

    target_ms = [row["r"].get("target_delay_ms", 0) for row in rows]
    buf_ms = [row["r"].get("current_buffer_size_ms", 0) for row in rows]
    preferred_ms = [row["r"].get("preferred_buffer_size_ms", 0) for row in rows]

    # ------------------------------------------------------------------ plot --
    fig, axes = plt.subplots(
        4, 1, figsize=(13, 14), sharex=True, constrained_layout=True
    )

    # Panel 1: sender packet rate + stall indicator
    ax = axes[0]
    ax.set_title("Sender: packet emission rate and stalls")
    bar_color = [
        "#d62728" if ms > 100 else "#1f77b4" for ms in max_pkt_ms
    ]
    ax.bar(t, sent_delta, color=bar_color, label="packets sent / sec", zorder=2)
    ax.axhline(50, color="gray", lw=1, ls="--", label="target 50 pkt/s")
    ax.set_ylabel("packets / sec")
    ax.set_ylim(0, max(sent_delta) * 1.15 + 5)
    ax2 = ax.twinx()
    ax2.plot(t, max_pkt_ms, color="tomato", lw=1.5, marker="o", ms=4,
             label="max_pkt_interval (ms)")
    ax2.axhline(20, color="lightgray", lw=0.8, ls=":")
    ax2.set_ylabel("max packet interval (ms)", color="tomato")
    ax2.tick_params(axis="y", labelcolor="tomato")
    ax2.set_ylim(0)
    # combined legend
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, loc="upper right", fontsize=8)
    ax.text(
        0.01, 0.97,
        "red bars = stall tick (max_pkt_interval > 100 ms)",
        transform=ax.transAxes, fontsize=7, va="top", color="dimgray"
    )

    # Panel 2: input capture throughput
    ax = axes[1]
    ax.set_title("Sender: mic input capture throughput (% of 48 kHz)")
    ax.bar(t, capture_pct, color="steelblue", label="frames captured / sec %")
    ax.axhline(100, color="gray", lw=1, ls="--", label="100% = 48 kHz continuous")
    ax.set_ylabel("% of 48 kHz")
    ax.set_ylim(0, 115)
    ax.legend(fontsize=8)

    # Panel 3: receiver NetEq rates (cumulative %)
    ax = axes[2]
    ax.set_title(
        "Receiver: NetEq operation rates (cumulative Q14 fractions, lower is better)"
    )
    ax.plot(t, expand_pct, color="#d62728", lw=2, marker="o", ms=4,
            label="expand_rate % (PLC / concealment)")
    ax.plot(t, accel_pct, color="darkorange", lw=2, marker="s", ms=4,
            label="accelerate_rate % (time-stretch drain)")
    ax.set_ylabel("% of total output samples")
    ax.set_ylim(0, 105)
    ax.yaxis.set_major_formatter(ticker.FormatStrFormatter("%g%%"))
    ax.legend(fontsize=8)
    ax.text(
        0.01, 0.97,
        "Both are cumulative lifetime ratios — once PLC events accumulate they don't reset",
        transform=ax.transAxes, fontsize=7, va="top", color="dimgray"
    )

    # Panel 4: receiver jitter buffer state
    ax = axes[3]
    ax.set_title("Receiver: jitter buffer state")
    ax.fill_between(t, buf_ms, alpha=0.25, color="steelblue", label="current_buffer_ms")
    ax.plot(t, buf_ms, color="steelblue", lw=1.5, marker="o", ms=3)
    ax.plot(t, target_ms, color="navy", lw=2, ls="--", marker="^", ms=5,
            label="target_delay_ms")
    ax.plot(t, preferred_ms, color="purple", lw=1, ls=":", marker="v", ms=4,
            label="preferred_buffer_ms")
    ax.plot(t, [ms / 1000.0 * 48.0 for ms in conc_delta_ms],
            color="red", lw=1, ls="-.", marker="x", ms=5,
            label="concealment ms/sec ÷ 48")
    ax.set_ylabel("milliseconds")
    ax.set_xlabel("seconds into run")
    ax.set_ylim(0)
    ax.legend(fontsize=8)

    # summary annotation
    total_conc_s = conc_cumul[-1] / 48_000.0
    total_run_s = len(rows)
    final_expand_pct = expand_pct[-1]
    stall_ticks = sum(1 for ms in max_pkt_ms if ms > 100)
    fig.suptitle(
        f"{log_path.name}  —  "
        f"{total_run_s}s run,  "
        f"stall ticks (>100ms gap): {stall_ticks},  "
        f"total concealment: {total_conc_s:.1f}s ({100*total_conc_s/total_run_s:.0f}% of run),  "
        f"final expand_rate: {final_expand_pct:.0f}%",
        fontsize=10,
    )

    out_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_png, dpi=150)
    print(out_png)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

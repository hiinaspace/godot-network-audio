#!/usr/bin/env -S uv run
# /// script
# dependencies = [
#   "matplotlib>=3.8",
# ]
# ///
"""
Plot sender/receiver stats from a JSONL trace or fallback godot_demo.log file.

Usage:
  plot_demo_stats.py INPUT [OUTPUT_PNG]

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


def parse_trace_jsonl(path: Path) -> list[dict]:
    rows = []
    lines = path.read_text().splitlines()
    for line_no, line in enumerate(lines, start=1):
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            # Harness shutdown may truncate the last line if Godot is terminated mid-write.
            if line_no == len(lines):
                break
            raise
        rows.append(
            {
                "t_sec": max(0.0, row.get("mono_usec", 0)) / 1_000_000.0,
                "delta_sec": float(row.get("delta_sec", 0.0)),
                "r": row.get("receiver", {}),
                "s": row.get("sender", {}),
                "mode": row.get("input_mode", "unknown"),
            }
        )
    if rows:
        t0 = rows[0]["t_sec"]
        for row in rows:
            row["t_sec"] -= t0
    return rows


def load_rows(path: Path) -> list[dict]:
    if path.suffix == ".jsonl":
        return parse_trace_jsonl(path)
    rows = parse_log(path)
    if rows:
        for i, row in enumerate(rows, start=1):
            row["t_sec"] = float(i)
            row["delta_sec"] = 1.0
        return rows
    jsonl_candidate = path.with_name("demo_trace.jsonl")
    if jsonl_candidate.exists():
        return parse_trace_jsonl(jsonl_candidate)
    return []


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print("usage: plot_demo_stats.py LOGFILE [OUTPUT_PNG]", file=sys.stderr)
        return 2

    input_path = Path(sys.argv[1])
    out_png = (
        Path(sys.argv[2])
        if len(sys.argv) == 3
        else input_path.parent / "demo_stats.png"
    )

    rows = load_rows(input_path)
    if not rows:
        print(f"no demo stats found in {input_path}", file=sys.stderr)
        return 1

    t = [row.get("t_sec", float(i)) for i, row in enumerate(rows, start=1)]
    dt = [
        max(1e-6, row.get("delta_sec", 0.0) or 0.0)
        for row in rows
    ]

    # --- sender series ---
    sent_cumul = [row["s"].get("packets_sent", 0) for row in rows]
    sent_delta = [sent_cumul[0]] + [
        max(0, sent_cumul[i] - sent_cumul[i - 1]) for i in range(1, len(sent_cumul))
    ]
    sent_rate = [sent_delta[i] / dt[i] for i in range(len(sent_delta))]
    max_pkt_ms = [row["s"].get("max_packet_interval_ms", 0.0) for row in rows]
    max_tick_lag_ms = [row["s"].get("max_tick_lag_ms", 0.0) for row in rows]
    avg_tick_lag_ms = [row["s"].get("avg_tick_lag_ms", 0.0) for row in rows]
    worker_ticks = [row["s"].get("worker_ticks", 0) for row in rows]
    empty_ticks = [row["s"].get("worker_empty_pcm_ticks", 0) for row in rows]
    partial_ticks = [row["s"].get("worker_partial_pcm_ticks", 0) for row in rows]
    silent_ticks = [row["s"].get("silent_ticks", 0) for row in rows]
    with_packets_ticks = [row["s"].get("worker_ticks_with_packets", 0) for row in rows]

    def cumulative_delta(values: list[float]) -> list[float]:
        if not values:
            return []
        return [values[0]] + [
            max(0, values[i] - values[i - 1]) for i in range(1, len(values))
        ]

    empty_tick_rate = [d / dt[i] for i, d in enumerate(cumulative_delta(empty_ticks))]
    partial_tick_rate = [d / dt[i] for i, d in enumerate(cumulative_delta(partial_ticks))]
    silent_tick_rate = [d / dt[i] for i, d in enumerate(cumulative_delta(silent_ticks))]
    with_packets_rate = [
        d / dt[i] for i, d in enumerate(cumulative_delta(with_packets_ticks))
    ]

    captured_cumul = [row["s"].get("captured_input_frames", 0) for row in rows]
    captured_delta = [captured_cumul[0]] + [
        max(0, captured_cumul[i] - captured_cumul[i - 1])
        for i in range(1, len(captured_cumul))
    ]
    input_rate_hz = rows[-1]["s"].get("input_sample_rate_hz", 48_000) if rows else 48_000
    expected_frames_per_sec = max(1, float(input_rate_hz))
    capture_pct = [
        100.0 * (captured_delta[i] / dt[i]) / expected_frames_per_sec
        for i in range(len(captured_delta))
    ]

    # --- receiver series ---
    conc_cumul = [row["r"].get("concealed_samples", 0) for row in rows]
    conc_delta_ms = [conc_cumul[0] / 48.0] + [
        max(0, conc_cumul[i] - conc_cumul[i - 1]) / 48.0
        for i in range(1, len(conc_cumul))
    ]
    conc_rate_ms = [conc_delta_ms[i] / dt[i] for i in range(len(conc_delta_ms))]

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
        5, 1, figsize=(13, 17), sharex=True, constrained_layout=True
    )

    # Panel 1: sender packet rate + gap indicator
    ax = axes[0]
    ax.set_title("Sender: packet emission rate and packet-gap indicator")
    bar_color = [
        "#d62728" if ms > 100 else "#1f77b4" for ms in max_pkt_ms
    ]
    bar_width = max(0.01, (max(t) / max(1, len(t))) * 0.85) if t else 0.1
    ax.bar(t, sent_rate, width=bar_width, color=bar_color, label="packets sent / sec", zorder=2)
    ax.axhline(50, color="gray", lw=1, ls="--", label="target 50 pkt/s")
    ax.set_ylabel("packets / sec")
    ax.set_ylim(0, max(sent_rate) * 1.15 + 5)
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

    # Panel 2: worker timing and starvation indicators
    ax = axes[1]
    ax.set_title("Sender: worker timing and PCM availability")
    ax.plot(t, with_packets_rate, color="#1f77b4", lw=2, marker="o", ms=3,
            label="ticks with packets / sec")
    ax.plot(t, empty_tick_rate, color="#d62728", lw=1.5, marker="x", ms=4,
            label="empty PCM ticks / sec")
    ax.plot(t, partial_tick_rate, color="darkorange", lw=1.5, marker="s", ms=3,
            label="partial PCM ticks / sec")
    ax.plot(t, silent_tick_rate, color="purple", lw=1.5, marker="^", ms=3,
            label="silent/VAD ticks / sec")
    ax.axhline(50, color="gray", lw=1, ls="--", label="target 50 pacing ticks/s")
    ax.set_ylabel("ticks / sec")
    ax.set_ylim(0, max(with_packets_rate + empty_tick_rate + partial_tick_rate + silent_tick_rate + [50]) * 1.15 + 2)
    ax2 = ax.twinx()
    ax2.plot(t, avg_tick_lag_ms, color="seagreen", lw=1.5, ls="--",
             label="avg tick lag (ms)")
    ax2.plot(t, max_tick_lag_ms, color="black", lw=1.5, ls=":",
             label="max tick lag (ms)")
    ax2.set_ylabel("tick lag (ms)")
    ax2.set_ylim(0)
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, fontsize=8, loc="upper right")

    # Panel 3: input capture throughput
    ax = axes[2]
    ax.set_title("Sender: mic input capture throughput (% of input sample rate)")
    ax.bar(t, capture_pct, color="steelblue", label="frames captured / sec %")
    ax.axhline(100, color="gray", lw=1, ls="--", label="100% = continuous input")
    ax.set_ylabel("% of input rate")
    ax.set_ylim(0, max(115, max(capture_pct) * 1.1 if capture_pct else 115))
    ax.legend(fontsize=8)

    # Panel 4: receiver NetEq rates (cumulative %)
    ax = axes[3]
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

    # Panel 5: receiver jitter buffer state
    ax = axes[4]
    ax.set_title("Receiver: jitter buffer state")
    ax.fill_between(t, buf_ms, alpha=0.25, color="steelblue", label="current_buffer_ms")
    ax.plot(t, buf_ms, color="steelblue", lw=1.5, marker="o", ms=3)
    ax.plot(t, target_ms, color="navy", lw=2, ls="--", marker="^", ms=5,
            label="target_delay_ms")
    ax.plot(t, preferred_ms, color="purple", lw=1, ls=":", marker="v", ms=4,
            label="preferred_buffer_ms")
    ax.plot(t, conc_rate_ms,
            color="red", lw=1, ls="-.", marker="x", ms=5,
            label="concealment ms/sec")
    ax.set_ylabel("milliseconds")
    ax.set_xlabel("seconds into run")
    ax.set_ylim(0)
    ax.legend(fontsize=8)

    # summary annotation
    total_conc_s = conc_cumul[-1] / 48_000.0
    total_run_s = t[-1] if t else 0.0
    final_expand_pct = expand_pct[-1]
    stall_ticks = sum(1 for ms in max_pkt_ms if ms > 100)
    fig.suptitle(
        f"{input_path.name}  —  "
        f"{total_run_s}s run,  "
        f"stall ticks (>100ms gap): {stall_ticks},  "
        f"total concealment: {total_conc_s:.1f}s ({100*total_conc_s/max(total_run_s, 1e-6):.0f}% of run),  "
        f"final expand_rate: {final_expand_pct:.0f}%",
        fontsize=10,
    )

    out_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_png, dpi=150)
    print(out_png)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

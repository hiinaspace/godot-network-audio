#!/usr/bin/env -S uv run
# /// script
# dependencies = [
#   "matplotlib>=3.8",
# ]
# ///
"""
Plot sender/receiver stats from trace file(s).

Single-file mode (loopback — both roles in one process):
  plot_demo_stats.py TRACE [OUTPUT_PNG]

Two-file mode (iroh / two separate Godot processes):
  plot_demo_stats.py --sender SENDER_TRACE --receiver RECEIVER_TRACE [OUTPUT_PNG]

OUTPUT_PNG defaults to demo_stats.png next to the input file(s).
"""

from __future__ import annotations

import argparse
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
            # Harness shutdown may truncate the last line if Godot is killed mid-write.
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


def _cumulative_delta(values: list[float]) -> list[float]:
    if not values:
        return []
    return [values[0]] + [max(0, values[i] - values[i - 1]) for i in range(1, len(values))]


def _make_dt(rows: list[dict]) -> list[float]:
    return [max(1e-6, row.get("delta_sec", 0.0) or 0.0) for row in rows]


def extract_sender_series(rows: list[dict]) -> dict:
    dt = _make_dt(rows)

    # prefer emitted_packets (worker-side cumulative, works even with direct-send handler)
    # fall back to packets_sent (drain-side counter, used in loopback / pre-iroh builds)
    def emitted(row):
        v = row["s"].get("emitted_packets")
        if v is None or v == 0:
            v = row["s"].get("packets_sent", 0)
        return v

    sent_cumul = [emitted(r) for r in rows]
    sent_delta = _cumulative_delta(sent_cumul)
    sent_rate = [sent_delta[i] / dt[i] for i in range(len(sent_delta))]

    max_pkt_ms = [r["s"].get("max_packet_interval_ms", 0.0) for r in rows]
    max_tick_lag_ms = [r["s"].get("max_tick_lag_ms", 0.0) for r in rows]
    avg_tick_lag_ms = [r["s"].get("avg_tick_lag_ms", 0.0) for r in rows]
    worker_ticks = [r["s"].get("worker_ticks", 0) for r in rows]
    empty_ticks = [r["s"].get("worker_empty_pcm_ticks", 0) for r in rows]
    partial_ticks = [r["s"].get("worker_partial_pcm_ticks", 0) for r in rows]
    silent_ticks = [r["s"].get("silent_ticks", 0) for r in rows]
    with_packets_ticks = [r["s"].get("worker_ticks_with_packets", 0) for r in rows]

    empty_tick_rate = [d / dt[i] for i, d in enumerate(_cumulative_delta(empty_ticks))]
    partial_tick_rate = [d / dt[i] for i, d in enumerate(_cumulative_delta(partial_ticks))]
    silent_tick_rate = [d / dt[i] for i, d in enumerate(_cumulative_delta(silent_ticks))]
    with_packets_rate = [d / dt[i] for i, d in enumerate(_cumulative_delta(with_packets_ticks))]

    captured_cumul = [r["s"].get("captured_input_frames", 0) for r in rows]
    captured_delta = _cumulative_delta(captured_cumul)
    input_rate_hz = rows[-1]["s"].get("input_sample_rate_hz", 48_000) if rows else 48_000
    expected_frames_per_sec = max(1, float(input_rate_hz))
    capture_pct = [100.0 * (captured_delta[i] / dt[i]) / expected_frames_per_sec for i in range(len(captured_delta))]

    avg_capture_pull_ms = [r["s"].get("avg_capture_pull_interval_ms", 0.0) for r in rows]
    max_capture_pull_ms = [r["s"].get("max_capture_pull_interval_ms", 0.0) for r in rows]
    max_empty_poll_streak = [r["s"].get("max_empty_input_poll_streak", 0) for r in rows]
    avg_main_drain_ms = [r["s"].get("avg_main_drain_interval_ms", 0.0) for r in rows]
    max_main_drain_ms = [r["s"].get("max_main_drain_interval_ms", 0.0) for r in rows]
    max_drain_batch = [r["s"].get("max_drain_batch_packets", 0) for r in rows]

    return dict(
        dt=dt,
        sent_rate=sent_rate,
        max_pkt_ms=max_pkt_ms,
        max_tick_lag_ms=max_tick_lag_ms,
        avg_tick_lag_ms=avg_tick_lag_ms,
        worker_ticks=worker_ticks,
        empty_tick_rate=empty_tick_rate,
        partial_tick_rate=partial_tick_rate,
        silent_tick_rate=silent_tick_rate,
        with_packets_rate=with_packets_rate,
        capture_pct=capture_pct,
        avg_capture_pull_ms=avg_capture_pull_ms,
        max_capture_pull_ms=max_capture_pull_ms,
        max_empty_poll_streak=max_empty_poll_streak,
        avg_main_drain_ms=avg_main_drain_ms,
        max_main_drain_ms=max_main_drain_ms,
        max_drain_batch=max_drain_batch,
    )


def extract_receiver_series(rows: list[dict]) -> dict:
    conc_cumul = [r["r"].get("concealed_samples", 0) for r in rows]
    conc_delta_ms = [conc_cumul[0] / 48.0] + [
        max(0, conc_cumul[i] - conc_cumul[i - 1]) / 48.0
        for i in range(1, len(conc_cumul))
    ]
    dt = _make_dt(rows)
    conc_rate_ms = [conc_delta_ms[i] / dt[i] for i in range(len(conc_delta_ms))]

    expand_pct = [100.0 * r["r"].get("expand_rate", 0) / Q14_SCALE for r in rows]
    accel_pct = [100.0 * r["r"].get("accelerate_rate", 0) / Q14_SCALE for r in rows]
    preemptive_pct = [100.0 * r["r"].get("preemptive_rate", 0) / Q14_SCALE for r in rows]
    expand_ops = [r["r"].get("expand_per_sec", 0.0) for r in rows]
    accel_ops = [r["r"].get("accelerate_per_sec", 0.0) for r in rows]
    preemptive_ops = [r["r"].get("preemptive_expand_per_sec", 0.0) for r in rows]
    normal_ops = [r["r"].get("normal_per_sec", 0.0) for r in rows]

    target_ms = [r["r"].get("target_delay_ms", 0) for r in rows]
    buf_ms = [r["r"].get("current_buffer_size_ms", 0) for r in rows]
    preferred_ms = [r["r"].get("preferred_buffer_size_ms", 0) for r in rows]
    avg_enqueue_ms = [r["r"].get("avg_enqueue_interval_ms", 0.0) for r in rows]
    max_enqueue_ms = [r["r"].get("max_enqueue_interval_ms", 0.0) for r in rows]
    packets_per_sec = [r["r"].get("packets_per_sec", 0.0) for r in rows]
    queued_packets = [r["r"].get("queued_packets", 0) for r in rows]
    enqueued_cumul = [r["r"].get("enqueued_packets", 0) for r in rows]
    dropped_cumul = [r["r"].get("dropped_packets", 0) for r in rows]

    return dict(
        conc_cumul=conc_cumul,
        conc_rate_ms=conc_rate_ms,
        expand_pct=expand_pct,
        accel_pct=accel_pct,
        preemptive_pct=preemptive_pct,
        expand_ops=expand_ops,
        accel_ops=accel_ops,
        preemptive_ops=preemptive_ops,
        normal_ops=normal_ops,
        target_ms=target_ms,
        buf_ms=buf_ms,
        preferred_ms=preferred_ms,
        avg_enqueue_ms=avg_enqueue_ms,
        max_enqueue_ms=max_enqueue_ms,
        packets_per_sec=packets_per_sec,
        queued_packets=queued_packets,
        enqueued_cumul=enqueued_cumul,
        dropped_cumul=dropped_cumul,
    )


def plot(
    t_s: list[float],
    ss: dict,
    t_r: list[float],
    rs: dict,
    out_png: Path,
    title_prefix: str = "",
) -> None:
    fig, axes = plt.subplots(5, 1, figsize=(13, 17), constrained_layout=True)

    # Panel 0: sender packet rate + gap indicator
    ax = axes[0]
    ax.set_title("Sender: packet emission rate and packet-gap indicator")
    bar_color = ["#d62728" if ms > 100 else "#1f77b4" for ms in ss["max_pkt_ms"]]
    bar_width = max(0.01, (max(t_s) / max(1, len(t_s))) * 0.85) if t_s else 0.1
    ax.bar(t_s, ss["sent_rate"], width=bar_width, color=bar_color, label="packets emitted / sec", zorder=2)
    ax.axhline(50, color="gray", lw=1, ls="--", label="target 50 pkt/s")
    ax.set_ylabel("packets / sec")
    ax.set_ylim(0, max(ss["sent_rate"] + [50]) * 1.15 + 5)
    ax2 = ax.twinx()
    ax2.plot(t_s, ss["max_pkt_ms"], color="tomato", lw=1.5, marker="o", ms=4,
             label="max_pkt_interval (ms)")
    ax2.axhline(20, color="lightgray", lw=0.8, ls=":")
    ax2.set_ylabel("max packet interval (ms)", color="tomato")
    ax2.tick_params(axis="y", labelcolor="tomato")
    ax2.set_ylim(0)
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, loc="upper right", fontsize=8)
    ax.text(0.01, 0.97, "red bars = stall tick (max_pkt_interval > 100 ms)",
            transform=ax.transAxes, fontsize=7, va="top", color="dimgray")
    ax.set_xlabel("seconds (sender)")

    # Panel 1: worker timing and starvation indicators
    ax = axes[1]
    ax.set_title("Sender: worker timing and PCM availability")
    ax.plot(t_s, ss["with_packets_rate"], color="#1f77b4", lw=2, marker="o", ms=3,
            label="ticks with packets / sec")
    ax.plot(t_s, ss["empty_tick_rate"], color="#d62728", lw=1.5, marker="x", ms=4,
            label="empty PCM ticks / sec")
    ax.plot(t_s, ss["partial_tick_rate"], color="darkorange", lw=1.5, marker="s", ms=3,
            label="partial PCM ticks / sec")
    ax.plot(t_s, ss["silent_tick_rate"], color="purple", lw=1.5, marker="^", ms=3,
            label="silent/VAD ticks / sec")
    ax.axhline(50, color="gray", lw=1, ls="--", label="target 50 pacing ticks/s")
    ax.set_ylabel("ticks / sec")
    ax.set_ylim(0, max(ss["with_packets_rate"] + ss["empty_tick_rate"] +
                       ss["partial_tick_rate"] + ss["silent_tick_rate"] + [50]) * 1.15 + 2)
    ax2 = ax.twinx()
    ax2.plot(t_s, ss["avg_tick_lag_ms"], color="seagreen", lw=1.5, ls="--",
             label="avg tick lag (ms)")
    ax2.plot(t_s, ss["max_tick_lag_ms"], color="black", lw=1.5, ls=":",
             label="max tick lag (ms)")
    ax2.set_ylabel("tick lag (ms)")
    ax2.set_ylim(0)
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, fontsize=8, loc="upper right")
    ax.set_xlabel("seconds (sender)")

    # Panel 2: input capture throughput
    ax = axes[2]
    ax.set_title("Sender: mic input capture throughput and pull cadence")
    ax.bar(t_s, ss["capture_pct"], color="steelblue", label="frames captured / sec %")
    ax.axhline(100, color="gray", lw=1, ls="--", label="100% = continuous input")
    ax.set_ylabel("% of input rate")
    ax.set_ylim(0, max(115, max(ss["capture_pct"]) * 1.1 if ss["capture_pct"] else 115))
    ax2 = ax.twinx()
    ax2.plot(t_s, ss["avg_capture_pull_ms"], color="seagreen", lw=1.5, marker="o", ms=3,
             label="avg capture pull interval (ms)")
    ax2.plot(t_s, ss["max_capture_pull_ms"], color="black", lw=1.2, ls=":", marker="x", ms=4,
             label="max capture pull interval (ms)")
    ax2.plot(t_s, ss["avg_main_drain_ms"], color="darkorange", lw=1.2, ls="--",
             label="avg main drain interval (ms)")
    ax2.plot(t_s, ss["max_main_drain_ms"], color="crimson", lw=1.2, ls=":",
             label="max main drain interval (ms)")
    ax2.plot(t_s, ss["max_empty_poll_streak"], color="purple", lw=1.2, ls="--", marker="^", ms=3,
             label="max empty poll streak")
    ax2.plot(t_s, ss["max_drain_batch"], color="gray", lw=1.0, ls="-.",
             label="max drain batch packets")
    ax2.set_ylabel("ms / polls")
    ax2.set_ylim(0)
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, fontsize=8, loc="upper right")
    ax.set_xlabel("seconds (sender)")

    # Panel 3: receiver NetEq rates (cumulative %) + operation activity
    ax = axes[3]
    ax.set_title("Receiver: NetEq rates and operation activity")
    ax.plot(t_r, rs["expand_pct"], color="#d62728", lw=2, marker="o", ms=4,
            label="expand_rate % (PLC / concealment)")
    ax.plot(t_r, rs["accel_pct"], color="darkorange", lw=2, marker="s", ms=4,
            label="accelerate_rate % (time-stretch drain)")
    ax.plot(t_r, rs["preemptive_pct"], color="purple", lw=1.5, marker="^", ms=3,
            label="preemptive_rate %")
    ax.set_ylabel("% of total output samples")
    ax.set_ylim(0, 105)
    ax.yaxis.set_major_formatter(ticker.FormatStrFormatter("%g%%"))
    ax2 = ax.twinx()
    ax2.plot(t_r, rs["expand_ops"], color="#8b0000", lw=1.2, ls="--", label="expand ops/sec")
    ax2.plot(t_r, rs["accel_ops"], color="#ff8c00", lw=1.2, ls="--", label="accelerate ops/sec")
    ax2.plot(t_r, rs["preemptive_ops"], color="#4b0082", lw=1.2, ls="--",
             label="preemptive ops/sec")
    ax2.plot(t_r, rs["normal_ops"], color="#2e8b57", lw=1.2, ls=":", label="normal ops/sec")
    ax2.set_ylabel("operations / sec")
    ax2.set_ylim(0)
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, fontsize=8, loc="upper right")
    ax.text(0.01, 0.97,
            "Q14 rates are cumulative lifetime ratios; ops/sec is the more useful live signal",
            transform=ax.transAxes, fontsize=7, va="top", color="dimgray")
    ax.set_xlabel("seconds (receiver)")

    # Panel 4: receiver jitter buffer state
    ax = axes[4]
    ax.set_title("Receiver: jitter buffer state and arrival cadence")
    ax.fill_between(t_r, rs["buf_ms"], alpha=0.25, color="steelblue", label="current_buffer_ms")
    ax.plot(t_r, rs["buf_ms"], color="steelblue", lw=1.5, marker="o", ms=3)
    ax.plot(t_r, rs["target_ms"], color="navy", lw=2, ls="--", marker="^", ms=5,
            label="target_delay_ms")
    ax.plot(t_r, rs["preferred_ms"], color="purple", lw=1, ls=":", marker="v", ms=4,
            label="preferred_buffer_ms")
    ax.plot(t_r, rs["conc_rate_ms"], color="red", lw=1, ls="-.", marker="x", ms=5,
            label="concealment ms/sec")
    ax.plot(t_r, rs["queued_packets"], color="gold", lw=1.2, ls="--", marker=".", ms=4,
            label="queued_packets (instantaneous)")
    ax.set_ylabel("milliseconds / packets")
    ax.set_ylim(0)
    ax2 = ax.twinx()
    ax2.plot(t_r, rs["avg_enqueue_ms"], color="darkgreen", lw=1.2, ls="--",
             label="avg enqueue interval (ms)")
    ax2.plot(t_r, rs["max_enqueue_ms"], color="forestgreen", lw=1.2, ls=":",
             label="max enqueue interval (ms)")
    ax2.plot(t_r, rs["packets_per_sec"], color="teal", lw=1.2, ls="-.",
             label="packets_per_sec (NetEq internal)")
    ax2.set_ylabel("enqueue ms / pkt rate")
    ax2.set_ylim(0)
    h1, l1 = ax.get_legend_handles_labels()
    h2, l2 = ax2.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, fontsize=8)
    ax.set_xlabel("seconds (receiver)")

    # summary
    total_conc_s = rs["conc_cumul"][-1] / 48_000.0 if rs["conc_cumul"] else 0.0
    total_run_s = max(t_s[-1] if t_s else 0.0, t_r[-1] if t_r else 0.0)
    final_expand_pct = rs["expand_pct"][-1] if rs["expand_pct"] else 0.0
    stall_ticks = sum(1 for ms in ss["max_pkt_ms"] if ms > 100)
    dropped_total = rs["dropped_cumul"][-1] if rs["dropped_cumul"] else 0
    fig.suptitle(
        f"{title_prefix}"
        f"{total_run_s:.0f}s run  |  "
        f"stall ticks (>100ms gap): {stall_ticks}  |  "
        f"dropped packets: {dropped_total}  |  "
        f"total concealment: {total_conc_s:.2f}s ({100*total_conc_s/max(total_run_s, 1e-6):.1f}% of run)  |  "
        f"final expand_rate: {final_expand_pct:.1f}%",
        fontsize=9,
    )

    out_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_png, dpi=150)
    print(out_png)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("input", nargs="?", help="Single combined trace (loopback mode)")
    parser.add_argument("output", nargs="?", help="Output PNG path")
    parser.add_argument("--sender", metavar="TRACE", help="Sender trace (two-process mode)")
    parser.add_argument("--receiver", metavar="TRACE", help="Receiver trace (two-process mode)")
    args = parser.parse_args()

    two_file_mode = args.sender or args.receiver
    if two_file_mode and args.input and not args.output:
        # positional arg after flags is output
        out_png = Path(args.input)
        args.input = None
    elif two_file_mode:
        out_png = Path(args.output) if args.output else None
    else:
        if not args.input:
            parser.print_help()
            return 2
        out_png = Path(args.output) if args.output else Path(args.input).parent / "demo_stats.png"

    if two_file_mode:
        sender_path = Path(args.sender) if args.sender else None
        receiver_path = Path(args.receiver) if args.receiver else None
        if out_png is None:
            ref = sender_path or receiver_path
            out_png = ref.parent / "demo_stats.png"

        sender_rows = parse_trace_jsonl(sender_path) if sender_path else []
        receiver_rows = parse_trace_jsonl(receiver_path) if receiver_path else []

        if not sender_rows and not receiver_rows:
            print("no trace data found", file=sys.stderr)
            return 1

        t_s = [r["t_sec"] for r in sender_rows]
        t_r = [r["t_sec"] for r in receiver_rows]
        title_prefix = f"iroh two-process  |  "
    else:
        rows = load_rows(Path(args.input))
        if not rows:
            print(f"no demo stats found in {args.input}", file=sys.stderr)
            return 1
        sender_rows = receiver_rows = rows
        t_s = t_r = [r["t_sec"] for r in rows]
        title_prefix = f"{Path(args.input).name}  —  "

    ss = extract_sender_series(sender_rows) if sender_rows else extract_sender_series([{"s": {}, "delta_sec": 1.0}])
    rs = extract_receiver_series(receiver_rows) if receiver_rows else extract_receiver_series([{"r": {}, "delta_sec": 1.0}])

    plot(t_s, ss, t_r, rs, out_png, title_prefix=title_prefix)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

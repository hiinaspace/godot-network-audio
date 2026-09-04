#!/usr/bin/env python3

import json
import pathlib
import re
import statistics
import sys


def percentile(values, rank):
    if not values:
        return 0.0
    values = sorted(values)
    return values[(len(values) - 1) * rank // 100]


output_dir = pathlib.Path(sys.argv[1])
connected_peers = int(sys.argv[2])
active_speakers = int(sys.argv[3])
run_seconds = float(sys.argv[4])
rows = [json.loads(line) for line in (output_dir / "receiver_trace.jsonl").read_text().splitlines() if line]
last = rows[-1] if rows else {}
receivers = last.get("receivers", {})
deltas_ms = [row.get("delta_sec", 0.0) * 1000.0 for row in rows]
receiver_values = list(receivers.values())
all_receiver_values = [
    value
    for row in rows
    for value in row.get("receivers", {}).values()
]

time_text = (output_dir / "receiver_time.txt").read_text() if (output_dir / "receiver_time.txt").exists() else ""
user = re.search(r"User time \(seconds\): ([0-9.]+)", time_text)
system = re.search(r"System time \(seconds\): ([0-9.]+)", time_text)
rss = re.search(r"Maximum resident set size \(kbytes\): (\d+)", time_text)
log_text = (output_dir / "receiver.log").read_text(errors="replace")

summary = {
    "godot_version": next((line for line in log_text.splitlines() if line.startswith("Godot Engine")), ""),
    "connected_peers_requested": connected_peers,
    "active_speakers_requested": active_speakers,
    "run_seconds": run_seconds,
    "receive_streams": last.get("receive_stream_count", 0),
    "playing_streams": sum(bool(value.get("is_playing")) for value in receiver_values),
    "enqueued_packets": sum(int(value.get("enqueued_packets", 0)) for value in receiver_values),
    "queue_dropped_packets": sum(int(value.get("dropped_packets", 0)) for value in receiver_values),
    "concealed_samples": sum(int(value.get("concealed_samples", 0)) for value in receiver_values),
    "consecutive_receiver_failures": sum(int(value.get("consecutive_failures", 0)) for value in receiver_values),
    "max_current_buffer_ms": max((int(value.get("current_buffer_size_ms", 0)) for value in all_receiver_values), default=0),
    "max_target_delay_ms": max((int(value.get("target_delay_ms", 0)) for value in all_receiver_values), default=0),
    "frame_delta_ms_p95": percentile(deltas_ms, 95),
    "frame_delta_ms_p99": percentile(deltas_ms, 99),
    "frame_delta_ms_max": max(deltas_ms, default=0.0),
    "receiver_cpu_percent_of_one_core": ((float(user.group(1)) if user else 0.0) + (float(system.group(1)) if system else 0.0)) / run_seconds * 100.0,
    "receiver_max_rss_kib": int(rss.group(1)) if rss else 0,
    "receiver_error_lines": sum("ERROR:" in line or "SCRIPT ERROR:" in line for line in log_text.splitlines()),
    "transport": last.get("transport", {}),
}
print(json.dumps(summary, indent=2, sort_keys=True))

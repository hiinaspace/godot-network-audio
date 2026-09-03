#!/usr/bin/env python3
"""Summarize voice-mesh-bench JSON files as stable CSV."""

import csv
import json
import pathlib
import sys


FIELDS = [
    "participants",
    "talkers",
    "dtx",
    "mesh_connections",
    "active_receiver_count",
    "setup_wall_ms",
    "media_cpu_percent_of_one_core",
    "max_rss_kib",
    "sender_skipped_ticks",
    "sent_datagrams",
    "missing_datagrams",
    "outbound_mbit_per_second",
    "latency_us_p50",
    "latency_us_p95",
    "latency_us_p99",
    "latency_us_max",
    "receive_queue_delay_us_p95",
    "playout_skipped_ticks",
    "playout_deadline_miss_percent",
    "playout_lateness_us_max",
    "neteq_concealed_percent",
    "neteq_receiver_errors",
    "neteq_max_target_delay_ms",
]


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: summarize_voice_mesh.py OUTPUT_DIR")
    output_dir = pathlib.Path(sys.argv[1])
    rows = []
    for path in sorted(output_dir.glob("*p-*t-dtx-*.json")):
        with path.open(encoding="utf-8") as handle:
            rows.append(json.load(handle))
    rows.sort(key=lambda row: (row["participants"], row["talkers"], row["dtx"]))

    writer = csv.DictWriter(sys.stdout, fieldnames=FIELDS, extrasaction="ignore")
    writer.writeheader()
    writer.writerows(rows)


if __name__ == "__main__":
    main()

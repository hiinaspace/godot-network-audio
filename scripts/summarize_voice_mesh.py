#!/usr/bin/env python3
"""Summarize voice-mesh-bench JSON files as stable CSV."""

import csv
import json
import pathlib
import sys


FIELDS = [
    "schema_version",
    "metric_sample_capacity",
    "scenario",
    "delivery",
    "receiver_policy",
    "interest_profile",
    "media_impairment",
    "media_loss_percent",
    "media_burst_ms",
    "media_outage_start_ms",
    "media_outage_duration_ms",
    "churn_profile",
    "churn_participant",
    "churn_start_ms",
    "churn_downtime_ms",
    "churn_disconnects",
    "churn_reconnects",
    "churn_reconnect_errors",
    "churn_reconnect_duration_ms",
    "affected_route_max_transport_gap_ms",
    "unaffected_route_max_transport_gap_ms",
    "seed",
    "runtime_worker_threads",
    "opus_version",
    "participants",
    "talkers",
    "interest_listeners",
    "max_interest_listeners",
    "dtx",
    "mesh_connections",
    "active_receiver_count",
    "setup_wall_ms",
    "media_cpu_percent_of_one_core",
    "max_rss_kib",
    "current_rss_kib_after_setup",
    "current_rss_kib_after_media",
    "sender_skipped_ticks",
    "stress_events",
    "stress_sender_ticks",
    "stress_sender_skipped_ticks",
    "stress_sent_datagrams",
    "stress_fanout_span_us_p95",
    "sender_callback_work_us_p95",
    "stress_sender_callback_work_us_p95",
    "fanout_span_us_p50",
    "fanout_span_us_p95",
    "fanout_span_us_max",
    "sent_datagrams",
    "accepted_datagrams",
    "outside_interest_datagrams",
    "media_impairment_attempted_datagrams",
    "media_impairment_delivered_datagrams",
    "media_impairment_dropped_datagrams",
    "missing_datagrams",
    "outbound_mbit_per_second",
    "latency_us_p50",
    "latency_us_p95",
    "latency_us_p99",
    "latency_us_max",
    "receive_queue_delay_us_p95",
    "interest_entry_to_first_media_us_p95",
    "interest_entry_events",
    "talkspurt_start_to_audio_us_p95",
    "talkspurt_audio_events",
    "playout_skipped_ticks",
    "playout_deadline_miss_percent",
    "stress_playout_deadline_miss_percent",
    "nonstress_playout_deadline_miss_percent",
    "stress_receive_queue_delay_us_p95",
    "listener_callback_work_us_p95",
    "stress_listener_callback_work_us_p95",
    "receive_drain_work_us_p95",
    "stress_receive_drain_work_us_p95",
    "playout_pull_work_us_p95",
    "stress_playout_pull_work_us_p95",
    "playout_lateness_us_max",
    "neteq_concealed_percent",
    "neteq_concealment_events",
    "neteq_silent_concealed_samples",
    "neteq_late_packets_discarded",
    "neteq_inserted_samples_for_deceleration",
    "neteq_removed_samples_for_acceleration",
    "neteq_receiver_errors",
    "neteq_max_current_buffer_ms",
    "neteq_max_target_delay_ms",
    "neteq_target_delay_observations",
    "neteq_target_delay_ge_100_ms_percent",
    "neteq_target_delay_ge_150_ms_percent",
    "neteq_target_delay_ge_100_ms_max_continuous_ms",
    "neteq_target_delay_ge_150_ms_max_continuous_ms",
    "receiver_creations",
    "receiver_reuses",
    "receiver_retirements",
    "max_concurrent_receivers",
    "max_receiver_pool",
]


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: summarize_voice_mesh.py OUTPUT_DIR")
    output_dir = pathlib.Path(sys.argv[1])
    rows = []
    for path in sorted(output_dir.glob("*.json")):
        with path.open(encoding="utf-8") as handle:
            row = json.load(handle)
            if "schema_version" in row and "participants" in row:
                rows.append(row)
    rows.sort(
        key=lambda row: (
            row.get("scenario", "baseline"),
            row["participants"],
            row["talkers"],
            row.get("delivery", "full-broadcast"),
            row.get("interest_profile", "none"),
            row.get("media_impairment", "none"),
            row.get("media_loss_percent", 0),
            row.get("media_burst_ms", 0),
            row.get("media_outage_duration_ms", 0),
            row.get("seed", 0),
            row["dtx"],
        )
    )

    writer = csv.DictWriter(sys.stdout, fieldnames=FIELDS, extrasaction="ignore")
    writer.writeheader()
    writer.writerows(rows)


if __name__ == "__main__":
    main()

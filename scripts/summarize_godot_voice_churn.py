#!/usr/bin/env python3

import json
import pathlib
import re
import sys


def percentile(values, rank):
    if not values:
        return 0.0
    values = sorted(values)
    return values[(len(values) - 1) * rank // 100]


def stats_at(rows, peer_id, unix_usec, after=False):
    candidates = (row for row in rows if (row["unix_usec"] >= unix_usec) == after)
    selected = None
    for row in candidates:
        if after:
            selected = row
            break
        selected = row
    return (selected or {}).get("receivers", {}).get(peer_id, {})


output_dir = pathlib.Path(sys.argv[1])
peer_count = int(sys.argv[2])
active_speakers = int(sys.argv[3])
run_seconds = float(sys.argv[4])
rows = [
    json.loads(line)
    for line in (output_dir / "receiver_trace.jsonl").read_text().splitlines()
    if line
]
load_events = [
    json.loads(line)
    for line in (output_dir / "loadgen_events.jsonl").read_text().splitlines()
    if line
]
receiver_events = [
    json.loads(line)
    for line in (output_dir / "receiver_events.jsonl").read_text().splitlines()
    if line
]

receiver_by_kind_peer = {
    (event["event"], event["peer_id"]): event for event in receiver_events
}
first_output_latencies = []
missing_first_output = []
for event in (event for event in load_events if event["event"] == "talker_on"):
    output = receiver_by_kind_peer.get(("first_non_silent_output", event["peer_id"]))
    if output is None:
        missing_first_output.append(event["peer_id"])
    else:
        first_output_latencies.append((output["unix_usec"] - event["unix_usec"]) / 1000.0)

disconnect_latencies = []
missing_disconnect = []
scheduled_leaves = [event for event in load_events if event["event"] == "leave_requested"]
for event in scheduled_leaves:
    disconnected = receiver_by_kind_peer.get(("peer_disconnected", event["peer_id"]))
    if disconnected is None:
        missing_disconnect.append(event["peer_id"])
    else:
        disconnect_latencies.append((disconnected["unix_usec"] - event["unix_usec"]) / 1000.0)

def last_output_tail_ms(event, end_usec):
    peer = event["peer_id"]
    previous = int(stats_at(rows, peer, event["unix_usec"]).get("non_silent_output_frames", 0))
    last_change = None
    for row in rows:
        if row["unix_usec"] < event["unix_usec"] or row["unix_usec"] > end_usec:
            continue
        value = int(row.get("receivers", {}).get(peer, {}).get("non_silent_output_frames", previous))
        if value > previous:
            last_change = row["unix_usec"]
        previous = max(previous, value)
    return max(0.0, ((last_change or event["unix_usec"]) - event["unix_usec"]) / 1000.0)


graceful_tails = []
for event in (event for event in load_events if event["event"] == "talker_off"):
    later_events = [
        other["unix_usec"]
        for other in load_events
        if other["peer_id"] == event["peer_id"]
        and other["unix_usec"] > event["unix_usec"]
        and other["event"] in ("talker_on", "leave_requested", "shutdown_requested")
    ]
    end_usec = min(later_events, default=rows[-1]["unix_usec"])
    graceful_tails.append(last_output_tail_ms(event, end_usec))

abrupt_tails = []
for event in (event for event in scheduled_leaves if event.get("active")):
    disconnected = receiver_by_kind_peer.get(("peer_disconnected", event["peer_id"]))
    end_usec = disconnected["unix_usec"] if disconnected else rows[-1]["unix_usec"]
    abrupt_tails.append(last_output_tail_ms(event, end_usec))

peer_lifetime = {}
for row in rows:
    for peer, value in row.get("receivers", {}).items():
        lifetime = peer_lifetime.setdefault(peer, {})
        for key in (
            "enqueued_packets",
            "dropped_packets",
            "concealed_samples",
            "consecutive_failures",
            "mixed_output_frames",
            "non_silent_output_frames",
        ):
            lifetime[key] = max(int(lifetime.get(key, 0)), int(value.get(key, 0)))
for event in receiver_events:
    if event["event"] != "peer_disconnected":
        continue
    lifetime = peer_lifetime.setdefault(event["peer_id"], {})
    for key, value in event.get("details", {}).items():
        if key in (
            "enqueued_packets",
            "dropped_packets",
            "concealed_samples",
            "consecutive_failures",
            "mixed_output_frames",
            "non_silent_output_frames",
        ):
            lifetime[key] = max(int(lifetime.get(key, 0)), int(value))

# Group the two intentionally staggered leave waves and measure concealment on
# talkers that remain active while another group departs.
leave_batches = []
for event in scheduled_leaves:
    if not leave_batches or event["unix_usec"] - leave_batches[-1][-1]["unix_usec"] > 500_000:
        leave_batches.append([event])
    else:
        leave_batches[-1].append(event)
active_state = {}
collateral_concealed = 0
for batch in leave_batches:
    start = batch[0]["unix_usec"]
    leaving = {event["peer_id"] for event in batch}
    for event in load_events:
        if event["unix_usec"] > start:
            break
        if event["event"] == "talker_on":
            active_state[event["peer_id"]] = True
        elif event["event"] in ("talker_off", "leave_requested", "shutdown_requested"):
            active_state[event["peer_id"]] = False
    disconnects = [
        receiver_by_kind_peer.get(("peer_disconnected", event["peer_id"]))
        for event in batch
    ]
    end = max(
        (event["unix_usec"] for event in disconnects if event is not None),
        default=batch[-1]["unix_usec"],
    ) + 200_000
    for peer, active in active_state.items():
        if not active or peer in leaving:
            continue
        before = int(stats_at(rows, peer, start).get("concealed_samples", 0))
        after = int(stats_at(rows, peer, end, after=True).get("concealed_samples", before))
        collateral_concealed += max(0, after - before)

time_text = (output_dir / "receiver_time.txt").read_text()
user = re.search(r"User time \(seconds\): ([0-9.]+)", time_text)
system = re.search(r"System time \(seconds\): ([0-9.]+)", time_text)
rss = re.search(r"Maximum resident set size \(kbytes\): (\d+)", time_text)
log_text = (output_dir / "receiver.log").read_text(errors="replace")
deltas_ms = [row.get("delta_sec", 0.0) * 1000.0 for row in rows]
last = rows[-1] if rows else {}

summary = {
    "peer_slots_requested": peer_count,
    "active_speakers_requested": active_speakers,
    "run_seconds": run_seconds,
    "join_events": sum(event["event"] == "joined" for event in load_events),
    "scheduled_leave_events": len(scheduled_leaves),
    "receiver_connect_events": sum(event["event"] == "peer_connected" for event in receiver_events),
    "receiver_disconnect_events": sum(event["event"] == "peer_disconnected" for event in receiver_events),
    "talker_activations": len(first_output_latencies) + len(missing_first_output),
    "missing_first_output": len(missing_first_output),
    "first_output_latency_ms_p50": percentile(first_output_latencies, 50),
    "first_output_latency_ms_p95": percentile(first_output_latencies, 95),
    "first_output_latency_ms_max": max(first_output_latencies, default=0.0),
    "missing_scheduled_disconnect": len(missing_disconnect),
    "disconnect_latency_ms_p50": percentile(disconnect_latencies, 50),
    "disconnect_latency_ms_p95": percentile(disconnect_latencies, 95),
    "disconnect_latency_ms_max": max(disconnect_latencies, default=0.0),
    "graceful_audio_tail_ms_p95": percentile(graceful_tails, 95),
    "graceful_audio_tail_ms_max": max(graceful_tails, default=0.0),
    "abrupt_leave_extension_tail_ms_max": max(abrupt_tails, default=0.0),
    "collateral_concealed_samples": collateral_concealed,
    "concealed_samples": sum(value.get("concealed_samples", 0) for value in peer_lifetime.values()),
    "queue_dropped_packets": sum(value.get("dropped_packets", 0) for value in peer_lifetime.values()),
    "consecutive_receiver_failures": sum(value.get("consecutive_failures", 0) for value in peer_lifetime.values()),
    "enqueued_packets": sum(value.get("enqueued_packets", 0) for value in peer_lifetime.values()),
    "max_receive_streams": max((int(row.get("receive_stream_count", 0)) for row in rows), default=0),
    "final_receive_streams": int(last.get("receive_stream_count", 0)),
    "frame_delta_ms_p95": percentile(deltas_ms, 95),
    "frame_delta_ms_p99": percentile(deltas_ms, 99),
    "frame_delta_ms_max": max(deltas_ms, default=0.0),
    "receiver_cpu_percent_of_one_core": ((float(user.group(1)) if user else 0.0) + (float(system.group(1)) if system else 0.0)) / run_seconds * 100.0,
    "receiver_max_rss_kib": int(rss.group(1)) if rss else 0,
    "receiver_error_lines": sum("ERROR:" in line or "SCRIPT ERROR:" in line for line in log_text.splitlines()),
    "transport": last.get("transport", {}),
}
print(json.dumps(summary, indent=2, sort_keys=True))

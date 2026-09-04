# voice-mesh-bench

Headless scale harness for the transport and media core. Its direct topology
creates one local-only Iroh endpoint per participant and one QUIC connection
for every pair. Its static-star topology connects each participant to a
dedicated authoritative forwarding endpoint. Both send scheduled Opus
talkspurts over unreliable datagrams and run a separate NetEq receiver for
every active speaker/listener direction.

Godot and the GDExtension are intentionally not involved.

```sh
cargo run --release -p voice-mesh-bench -- \
  --participants 8 --talkers 2 --seconds 10 --dtx on \
  --output target/voice-mesh/8p-2t-dtx-on.json
```

The JSON includes connection/setup cost, process CPU and peak RSS, transport
delivery and bitrate, application-observed one-way latency, receive-queue delay,
10 ms playout deadline misses, and aggregate NetEq health. Latency uses one
process-wide monotonic clock, so it can also verify that a `tc netem` rule is on
the actual Iroh packet path.

The first baseline matrix is 4/8/16/32 participants, 1 and 2 talkers, with DTX
on and off. All participants remain fully connected even though only the
scheduled talkers send media.

Run that matrix and produce a CSV summary with:

```sh
scripts/run_voice_mesh_baseline.sh target/voice-mesh/baseline-current 10
```

The first game-shaped scenario keeps the complete connection mesh but rotates
conversation ownership through every participant and changes each speaker's
listener set on a deterministic timeline. Compare sender-side filtering with
full broadcast followed by receiver-side interest discard using:

```sh
scripts/run_voice_game_interest.sh target/voice-mesh/game-interest 30 5
```

Individual runs accept `--scenario game-interest`, `--delivery
sender-filtered|broadcast-discard`, `--receiver-policy retire|pool`,
`--interest-profile rotating|crowd-burst|group-merge|boundary-oscillation`,
`--interest-listeners N`, and `--seed N`. The matrix script also accepts
`RECEIVER_POLICY=retire|pool` (default `retire`) and `INTEREST_PROFILE`.
Result JSON includes per-participant metrics, receiver lifecycle counts,
interest-entry-to-media delay, and talkspurt-start-to-audible-output delay.

Run the correlated 32-participant stress profiles with bounded receiver reuse:

```sh
scripts/run_voice_interest_stress.sh target/voice-mesh/interest-stress 36 5
```

This compares the rotating control with a one-second all-speaker burst, a
split-to-merged group transition, and 100 ms oscillation between disjoint
listener sets. Stress-window sender, fanout, queue, and playout metrics are
reported separately from the full-run aggregates. Virtual participants use
independent, phase-staggered sender tasks. `RUNTIME_WORKERS=N` controls the
shared Tokio runtime for diagnosing harness contention; it does not model
additional client machines.

On glibc systems, schema-v4 RSS samples also include allocator arena, in-use,
free, and mmap totals. These counters are diagnostic and report zero on other
platforms.

Schema v5 keeps exact metric event counts and maxima, but computes percentiles
from mergeable deterministic samples capped at 4,096 observations per metric.
This prevents the benchmark's diagnostics from growing with run duration; the
JSON records the cap as `metric_sample_capacity`.

Run deterministic loss/burst/outage treatments after Iroh receipt and before
NetEq insertion with:

```sh
scripts/run_voice_media_impairment.sh target/voice-mesh/media-impairment 10 3
```

These schema-v6 results are the **media-boundary lane**: injected drops are
reported separately and `missing_datagrams` should remain zero. They measure
receiver/playout behavior, not Iroh robustness. Schema v6 also records a 1 Hz,
per-participant NetEq buffer/target/concealment timeline (capped at one hour).

Run the distinct Iroh transport lane on `gna-sim` with:

```sh
scripts/run_voice_transport_netem.sh target/voice-mesh/transport-netem 10
```

The runner refuses to replace an unexpected loopback qdisc, checks that shaped
profiles count packets, verifies application-observed delay, and restores
loopback to `noqueue` even on failure.

Run the game-shaped static-delay and clean-impaired-clean recovery lane with:

```sh
scripts/run_voice_recovery_netem.sh target/voice-mesh/recovery-netem 24 3
```

Schema v7 measures the percentage of active NetEq playout observations at or
above 100 ms and 150 ms target delay, plus the longest continuous interval at
each threshold. Intentional DTX silence is excluded. The recovery runner records
its transition times and qdisc counters and restores loopback on success or
failure.

Run real Iroh connection churn with continuous media using:

```sh
scripts/run_voice_churn.sh target/voice-mesh/churn 12 3
```

Schema v9 covers late join, permanent leave, same-identity reconnect, and
new-identity replacement. Affected and unaffected route gaps are reported
separately. The churn runner uses all participants as continuous senders so DTX
and interest changes cannot be mistaken for transport gaps.

Compare the sender-filtered direct mesh with a static authoritative voice star:

```sh
scripts/run_voice_topology_comparison.sh target/voice-mesh/topology 12 3
```

Schema v10 adds `--topology direct|star`, SFU ingress/forwarding/error counters,
and separate client-uplink and SFU-egress bitrates. The SFU forwards the
original encoded datagram without decoding or mixing, filtering against the
same deterministic interest schedule used by direct senders.

Use `--process-layout multi` to run each virtual participant as an independent
OS process (and the authoritative forwarder as another process for star). This
schema-v11 control retains one encoder, packet queue, NetEq set, and playback
clock per client, while removing the shared Tokio runtime and heap. Its report
includes summed worker CPU/RSS, maximum per-worker RSS, merged latency samples,
and aggregate transport, deadline, concealment, and SFU counters. Multiprocess
latency uses the host-wide monotonic clock; this mode is currently scoped to the
game-interest scenario without churn.

Each virtual listener owns its own packet queue, NetEq instances, and staggered
10 ms playback clock. This preserves the distributed cost shape of a party
while still running all simulated clients inside one process and one pod.

See [PLAN.md](PLAN.md) for the research-informed positional-interest,
impairment, topology, churn, and perceptual-quality roadmap.
The first direct-delivery results are recorded in
[GAME_INTEREST.md](GAME_INTEREST.md).

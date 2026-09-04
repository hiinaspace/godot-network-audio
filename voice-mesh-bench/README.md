# voice-mesh-bench

Headless, single-process scale harness for the transport and media core. It
creates one local-only Iroh endpoint per participant, establishes one QUIC
connection for every pair, sends scheduled Opus talkspurts over unreliable
datagrams, and runs a separate NetEq receiver for every active
speaker/listener direction.

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

Each virtual listener owns its own packet queue, NetEq instances, and staggered
10 ms playback clock. This preserves the distributed cost shape of a party
while still running all simulated clients inside one process and one pod.

See [PLAN.md](PLAN.md) for the research-informed positional-interest,
impairment, topology, churn, and perceptual-quality roadmap.
The first direct-delivery results are recorded in
[GAME_INTEREST.md](GAME_INTEREST.md).

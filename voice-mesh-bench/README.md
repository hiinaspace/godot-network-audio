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

Each virtual listener owns its own packet queue, NetEq instances, and staggered
10 ms playback clock. This preserves the distributed cost shape of a party
while still running all simulated clients inside one process and one pod.

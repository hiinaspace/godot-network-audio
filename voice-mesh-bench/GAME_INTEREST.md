# Game-interest direct-delivery results

Date: 2026-09-03
Runner: `gna-sim`, 8-CPU quota, 20 GiB memory limit

## Scenario

The first game-shaped workload keeps a complete Iroh connection mesh while all
participants maintain Opus encoders. Four conversation slots rotate talker
ownership through every participant. Transitions deterministically alternate
between a 300 ms gap, a clean handoff, and a 100 ms overlap. Each speaker's
interest set changes every three seconds.

At 8 and 16 participants each speaker has three interested listeners; at 32 it
has seven. Receivers are created lazily on the first interested packet. The
initial matrix retired them immediately when interest ended. Two delivery
modes use the identical activity and interest schedule:

- `sender-filtered` sends only to interested listeners;
- `broadcast-discard` sends to every peer and discards outside interest before
  NetEq insertion.

The initial matrix used five deterministic seeds and 10-second runs. All 30
runs completed with zero missing datagrams, send errors, or NetEq errors. The
cgroup did not throttle during this matrix.

| Participants | Delivery | Median sent datagrams | Median Mbit/s | Median CPU, one-core % | Median RSS MiB | Median deadline misses | Median talkspurt-to-audio p95 |
|---:|:---|---:|---:|---:|---:|---:|---:|
| 8  | filtered  | 7,152   | 0.346 | 97.5  | 33.0  | 6.33% | 43.6 ms |
| 8  | broadcast | 16,688  | 0.808 | 117.3 | 33.7  | 6.74% | 44.7 ms |
| 16 | filtered  | 8,484   | 0.391 | 129.3 | 57.3  | 2.84% | 47.3 ms |
| 16 | broadcast | 42,420  | 1.957 | 169.6 | 59.9  | 2.72% | 46.2 ms |
| 32 | filtered  | 26,096  | 1.092 | 186.8 | 171.5 | 4.89% | 48.6 ms |
| 32 | broadcast | 113,708 | 4.762 | 253.1 | 181.0 | 6.11% | 40.4 ms |

The strict deadline metric counts a virtual 10 ms callback as late at more than
2 ms. Per-participant tails were substantially worse than aggregate values: in
the first 32-participant matrix, the worst participant reached 17.4% deadline
misses. This is a scheduling diagnostic, not an assertion that the same rate is
directly audible.

## 32-participant fanout repeat

The 32-participant cases were repeated for five seeds after adding direct
fanout-span measurements. Results remained error-free.

| Metric | Sender-filtered median | Broadcast-discard median |
|:---|---:|---:|
| Sent datagrams | 26,096 | 114,328 |
| Accepted datagrams | 26,095 | 25,816 |
| CPU, one-core % | 183.9 | 249.4 |
| Current/lifetime peak RSS | about 172 MiB | about 181 MiB |
| Fanout span p50 / p95 | 8 / 26 us | 26 / 58 us |
| Fanout span maximum, median across runs | 106 us | 253 us |
| Sender skipped ticks | 1 | 7 |
| Transport latency p95 | 1.8 ms | 4.1 ms |
| Aggregate deadline misses | 4.68% | 6.85% |
| Worst participant deadline misses | 14.8% | 18.7% |

Fanout calls are far shorter than the 20 ms send period, so the broadcast
sender is not blocking inside one fanout loop. The higher skipped-tick and
latency tails instead correlate with whole-process scheduler pressure from
roughly 4.4 times as many datagrams. Broadcast also produced slightly fewer
encoded/accepted packets because its sender task skipped more scheduled ticks.

The sender-filtered receiver may reject a handful of packets at an interest
boundary if they were valid when sent but arrive after that listener has left
interest. That is recorded as `outside_interest_datagrams`, not transport loss.

## Soak and receiver-lifecycle finding

A 600-second, 32-participant sender-filtered soak delivered all 1,463,966 sent
datagrams with zero send or NetEq errors and no cgroup throttling. It created
34,430 receivers, retired 34,322, and ended with 108 active. CPU (187% of one
core), transport p95 (1.76 ms), deadline-miss rate (4.67%), and fanout p95
(27 us) remained close to the short runs.

Peak RSS nevertheless reached about 222 MiB, versus roughly 172 MiB in the
10-second runs. A follow-up 180-second run sampled current RSS every ten
seconds: it rose from 121 MiB after setup to 173 MiB at 10 seconds, 185 MiB at
60 seconds, 188 MiB at 90 seconds, and 195 MiB at 180 seconds. Combined with
the ten-minute high-water, this looks sublinear but is not yet a demonstrated
plateau.

## Immediate retirement versus bounded reuse

The harness now supports both `--receiver-policy retire` and `pool`. A pooled
receiver is flushed and given a fresh Opus decoder before it represents a new
logical stream. Pools are local to each listener and bounded by that
listener's peak inactive receiver count, rather than allocating every possible
direction eagerly.

A matched 60-second, 32-participant sender-filtered comparison used seed 44.
Both policies delivered every datagram and had zero send and NetEq errors.

| Metric | Immediate retirement | Bounded pool |
|:---|---:|---:|
| Sent/received datagrams | 148,064 / 148,064 | 148,344 / 148,344 |
| Receiver constructions | 3,509 | 357 |
| Receiver reuses | 0 | 3,152 |
| CPU, one-core % | 188.6 | 163.2 |
| Current RSS after media | 185.1 MiB | 217.2 MiB |
| Aggregate deadline misses | 5.24% | 2.83% |
| Sender skipped ticks | 5 | 0 |
| Concealed samples | 20,896,800 (3.39%) | 11,580,960 (1.87%) |
| Talkspurt-to-audio p95 | 54.0 ms | 55.6 ms |

Pooling therefore removes most receiver construction work and improves CPU and
scheduling tails, but deliberately retains about 32 MiB more memory at this
point in the run. The similar talkspurt latency and lower concealment give no
sign of stale decoder state after reset. Keep both policies available: the
pool is the useful stress-test baseline, while immediate retirement remains a
memory-minimizing comparison. A longer pooled run is still needed to establish
its steady RSS ceiling; neither result should yet be described as a leak.

Receiver concealment values in results generated before the lifecycle
accounting fix counted only receivers still alive at the end. Transport,
timing, CPU, and memory values from those runs remain usable, but their
concealment percentages are not comparable with the matched run above.

Raw results are on `gna-sim` under:

```text
/work/projects/godot-network-audio/target/voice-mesh/game-interest-v1/
/work/projects/godot-network-audio/target/voice-mesh/game-interest-v2-32/
/work/projects/godot-network-audio/target/voice-mesh/game-interest-soak/
```

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
receiver is flushed and its Opus decoder state reset before it represents a
new logical stream. Pools are local to each listener and bounded by that
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

## Correlated-interest stress

The first stress matrix below used the schema-v2 sender implementation, which
encoded every virtual participant serially in one periodic task. Its delivery,
traffic, lifecycle, and error counts remain valid. Its callback-deadline rates
are useful as a single-process saturation signal, but are not a faithful model
of independently clocked desktop senders; the harness-fidelity follow-up below
corrects that design.

Three deterministic profiles were compared with the rotating control at 32
participants, sender-filtered delivery, and pooled receivers. Each of five
36-second seeds contained three stress events:

- `crowd-burst`: every participant speaks for one second;
- `group-merge`: four groups of eight pair into two groups of sixteen, raising
  each speaker's listener fanout from 7 to 15, then split again;
- `boundary-oscillation`: every 100 ms, all speakers switch between two
  disjoint seven-listener sets for four seconds.

All 20 runs delivered all 2,104,012 datagrams with zero send, malformed, or
NetEq errors. The cgroup accumulated four throttle events totaling only 12 ms
over the roughly 12-minute matrix.

| Profile | Median sent | Median CPU | Median current RSS | Median deadline misses | Stress / non-stress deadline misses | Worst participant stress rate | Receiver constructions / reuses | Concealment |
|:---|---:|---:|---:|---:|---:|---:|---:|---:|
| Rotating control | 89,600 | 163.4% | 212.1 MiB | 2.78% | n/a / 2.78% | n/a | 360 / 1,790 | 2.66% |
| Crowd burst | 118,496 | 183.9% | 210.9 MiB | 4.39% | 18.88% / 3.08% | 28.0% | 360 / 1,776 | 3.62% |
| Group merge | 122,908 | 182.7% | 231.4 MiB | 4.12% | 6.54% / 2.72% | 15.33% | 480 / 512 | 1.55% |
| Boundary oscillation | 89,684 | 180.0% | 194.6 MiB | 3.50% | 3.13% / 3.74% | 14.17% | 261 / 10,474 | 5.72% |

The crowd burst is the first clean-path workload to expose a pronounced
correlated scheduling failure: its stress-window deadline rate is about six
times its own non-stress rate. Fanout itself is still short (24 us stress p95),
so the likely pressure comes from simultaneously encoding, delivering, and
pulling 32 newly active streams. Group merge increases traffic and receiver
cardinality but produces a smaller deadline step. Boundary oscillation spends
more CPU and raises concealment through repeated stream startup/reset, yet does
not make its stress-window deadline rate worse than its non-stress rate.

These deadlines are virtual callback diagnostics, not direct audible-failure
counts.

### Harness-fidelity follow-up

Schema v3 gives each virtual participant an independent, phase-staggered sender
task and reports callback work time. In a three-seed 32-participant rerun on
eight Tokio workers, the rotating control used 282% of one core and had 6.65%
deadline misses. The crowd case used 294%, with 31.17% misses in the stress
window versus 5.86% outside it. The formerly batched sender was not the direct
cause: individual sender work was only 1.31 ms p95 in the control and 1.73 ms
in the burst. Per-listener callback work rose from 0.90 ms to 4.61 ms p95 in
the burst, almost entirely in NetEq pull/decode.

Increasing the shared runtime to 32 workers made contention worse: crowd CPU
rose to 408%, and stress-window deadline misses to 63.32%. This is not a
runtime-width shortage.

An endpoint-shaped control then used eight participants with all-to-all
interest. Every listener still received the same seven simultaneous speakers,
but the pod no longer simulated 24 unrelated desktops. Across three seeds it
used 140% of one core, had zero sender skips, and measured 5.29% stress-window
deadline misses versus 4.66% outside the burst. Sender work was 1.67 ms p95 and
listener NetEq pull work 1.75 ms p95 during the burst. Every datagram arrived
and no NetEq errors occurred.

The severe 32-participant callback spike is therefore predominantly aggregate
single-host scheduling contention, not evidence that one desktop cannot play
seven active speakers. Use the 32-participant process for Iroh traffic,
connection, and aggregate-resource stress; use the endpoint-shaped case for
per-client decoder/playout budgets. A later multiprocess or multi-host run is
required before treating 32-participant virtual callback deadlines as user
experience.

## Pooled-receiver RSS follow-up

The ten-minute, 32-participant pooled run remained transport-clean: all
1,468,138 datagrams arrived, with zero send/NetEq errors and zero cgroup
throttling. It used 155% of one core, skipped four aggregate sender ticks, and
had 2.66% aggregate deadline misses. Receiver allocation was bounded at 439
constructions, 33,991 reuses, and a maximum per-listener pool of 15.

Current RSS did not plateau. It rose from 121 MiB after setup to 239 MiB at 200
seconds and 277 MiB at 600 seconds. Replacing decoder reallocation with an
in-place Audiopus state reset reduced the matched 180-second endpoint only
from 236 MiB to 231 MiB, so decoder construction is not the main explanation.
The pool remains the lower-CPU policy, but memory growth needs allocator and
NetEq-buffer investigation before it becomes a production recommendation.

A schema-v4 180-second repeat sampled glibc `mallinfo2()` alongside RSS. RSS
rose from 122 to 234 MiB and allocator in-use space from 121 to 234 MiB. Free
arena space remained below 8 MiB and mmap allocation stayed flat at 92 MiB.
The growth is therefore live heap allocation, not glibc merely retaining a
large freed arena.

Heaptrack then attributed a 90-second, 32-participant run's largest retained
blocks to three bounded categories: about 96 MiB for 32 Iroh endpoint interface
discovery buffers, 51 MiB for 363 pooled Opus/NetEq receivers (about 141 KiB
each), and 13 MiB for benchmark timing vectors. The receiver cost is the
aggregate for 32 clients co-located in one process, or roughly 4-5 MiB per
client at this workload's high-water mark. The vectors were a harness artifact:
they retained every timing observation for the full run.

Schema v5 replaces those vectors with deterministic, mergeable bottom-k samples
capped at 4,096 observations per metric. Event counts and maxima remain exact.
In a corrected five-minute run, current RSS rose from 125 MiB to 209 MiB at 60
seconds, 219 MiB at 180 seconds, and 223 MiB at 300 seconds. Growth was about 4
MiB over the final two minutes, consistent with bounded receiver-pool warm-up.
All 734,692 datagrams arrived with no send or NetEq errors, and CPU remained at
247% of one core, essentially unchanged from the schema-v4 180-second run.

## Initial impairment lanes

Schema v6 separates deterministic loss after Iroh receipt from transport-path
`tc netem`. In three-seed, 8-participant media-boundary runs, median clean
concealment was 4.49% because rotating interest itself repeatedly warms new
receivers. Uniform 3% loss raised it to 6.84%; 3% loss in 60 ms mean bursts
raised it to 7.79%. A 100 ms outage was nearly absorbed at 4.95%, while 300 ms
and 1000 ms outages reached 8.10% and 10.19%. Every transport datagram arrived,
and all cases had zero NetEq errors.

The first transport-path sweep independently validated loopback netem. A 40±10
ms rule moved median one-way latency from 0.4 ms to 40.6 ms. Static 1% and 3%
loss produced 68 and 185 missing application datagrams; the burst profile
produced 191. Kernel qdisc counters recorded 106, 312, and 319 drops because
they also include QUIC/control packets. The runner restored `lo` to `noqueue`.

The new 1 Hz timeline exposed a 1430 ms receiver buffer peak in a 5% uniform-loss
seed despite a configured 250 ms maximum. A direct NetEq-only periodic-loss
reproduction remained bounded (162 ms peak and 17 ms final), which localized
the runaway behavior to `VoiceReceiver`: after an end-of-talkspurt marker, loss
of the next unreliable start marker left the receiver in intentional silence.
Later voiced packets entered NetEq but playout did not drain them. The receiver
now treats a newer nonempty packet as an implicit talkspurt start. With the same
seed and the same 1045 deterministic drops, the overall buffer peak fell from
1430 ms to 226 ms, and the post-startup peak fell from 1430 ms to 96 ms. Both
runs had zero transport or NetEq errors.

The investigation also found a separate issue suitable for the NetEq fork:
delay estimation discarded each packet's recorded monotonic arrival time and
used `Instant::now()` at insertion instead. A focused regression with perfectly
paced transport timestamps and temporary listener scheduling stalls produced a
60 ms target on the old code and 20 ms after the fix. The fork now propagates
the packet arrival time through delay tracking and history expiry. This avoids
classifying application scheduling delay as network jitter.

A schema-v7 follow-up localized the remaining 220-250 ms target-delay tail to a
second fork issue rather than loss adaptation. The first packet recomputed the
80 ms startup target from an empty delay histogram, whose uninitialized
quantile selected the configured maximum until the first 500 ms resample. Fork
commit `69c6ccb` preserves the startup target until a real sample exists. A
clean two-second smoke changed from about half its active observations at or
above 150 ms to zero, with a 20 ms final target.

## Static jitter and recovery follow-up

The guarded schema-v7 runner used three 24-second seeds per profile. Recovery
runs were clean for six seconds, impaired for eight, then clean for ten. Target
occupancy excludes intentional DTX silence, and buffer maxima are now sampled
on every 10 ms playout callback rather than only at receiver retirement.

| Profile | Median latency p50 / p95 | Median missing datagrams | Median concealment | Worst buffer | Worst target |
|:---|---:|---:|---:|---:|---:|
| Clean | 0.34 / 0.58 ms | 0 | 0.94% | 118 ms | 80 ms |
| Static 40±10 ms | 40.22 / 56.50 ms | 0 | 1.72% | 130 ms | 80 ms |
| Static 80±30 ms | 80.07 / 128.51 ms | 0 | 3.29% | 183 ms | 100 ms |
| Recovery 60±20 ms + 1% loss | 0.44 / 80.37 ms | 222 | 2.41% | 148 ms | 80 ms |
| Recovery 40±10 ms + burst loss | 0.43 / 50.50 ms | 470 | 2.90% | 128 ms | 80 ms |

Only one 80±30 ms seed reached 100 ms target delay: 0.63% of its active
observations, with a longest continuous interval of 1.27 seconds. No run
reached 150 ms. After impairment removal, one-second buffer samples returned
to at most 93 ms in the delay/loss profile and 84 ms in the burst profile. All
15 runs had zero NetEq errors, pure-jitter profiles lost no application
datagrams, every shaped profile had nonzero qdisc counters, and loopback was
restored to `noqueue`.

## Direct-mesh lifecycle churn

Schema v8 replaces live connection handles so a participant can leave and
reconnect without stopping sender or listener tasks. The first matrix used
eight continuous, non-DTX senders for 12 seconds, disconnected participant 0
at four seconds, and tested three downtime values across three seeds. Keeping
every route active makes transport gaps independent of game-interest and
talkspurt schedules.

| Downtime | Median reconnect | Median affected-route maximum gap | Worst unaffected-route gap | Median send errors while absent | Median concealment |
|---:|---:|---:|---:|---:|---:|
| none | n/a | n/a | 46 ms | 0 | 0.53% |
| 250 ms | 16 ms | 299 ms | 46 ms | 187 | 1.10% |
| 1000 ms | 20 ms | 1059 ms | 47 ms | 716 | 2.67% |
| 3000 ms | 28 ms | 3059 ms | 54 ms | 2116 | 6.83% |

Every churn run closed and rebuilt all seven participant connections with zero
reconnect errors. Three datagrams total were accepted for sending immediately
before a connection close but did not arrive; all other successful sends were
delivered. Unaffected-route gaps remained close to the clean scheduler tail,
with no evidence that reconnecting one peer disrupted the rest of the mesh.
NetEq reported zero errors, target delay stayed at or below 80 ms, and the worst
buffer peak was 127 ms.

Schema v9 then added the distinct lifecycle cases. Across three seeds, a late
participant created a fresh endpoint and all seven connections in a median 7
ms; same-identity reconnect took 5 ms; and new-identity replacement took 31
ms. Every requested link succeeded without reconnect errors. Permanent leave
remained absent through the end as intended. Unaffected gaps stayed below 48
ms, only two in-flight datagrams were lost across the complete lifecycle
matrix, and NetEq stayed error-free at an 80 ms target.

## Static authoritative star

Schema v10 compares the sender-filtered direct mesh with an extra static Iroh
endpoint that forwards unchanged encoded datagrams to interested listeners.
Three 12-second seeds used four scheduled talkers, seven listeners per talker,
DTX, and pooled receivers at each size. All direct and star runs delivered the
exact intended downstream count with zero send, SFU, transport, or NetEq
errors.

| N | Topology | Connections | Setup | CPU | RSS | Client uplink | SFU egress | Latency p50 / p95 |
|---:|:---|---:|---:|---:|---:|---:|---:|---:|
| 8 | direct | 28 | 35 ms | 155% | 36.6 MiB | 0.91 Mbit/s | n/a | 0.34 / 0.57 ms |
| 8 | star | 8 | 19 ms | 158% | 35.5 MiB | 0.13 Mbit/s | 0.91 Mbit/s | 0.49 / 0.84 ms |
| 16 | direct | 120 | 121 ms | 201% | 74.3 MiB | 1.02 Mbit/s | n/a | 0.31 / 0.68 ms |
| 16 | star | 16 | 34 ms | 215% | 51.6 MiB | 0.15 Mbit/s | 1.03 Mbit/s | 0.52 / 0.99 ms |
| 32 | direct | 496 | 464 ms | 297% | 185.5 MiB | 1.24 Mbit/s | n/a | 0.36 / 4.69 ms |
| 32 | star | 32 | 68 ms | 301% | 90.2 MiB | 0.18 Mbit/s | 1.24 Mbit/s | 0.56 / 4.95 ms |

The star does not remove aggregate encryption or send work, so total CPU is
similar. It does move fanout off clients, reduce client uplink by roughly 7x,
and sharply reduce connection/setup state. The additional forwarding hop costs
about 0.2 ms at low load; at N=32 both p95s are dominated by co-located host
scheduling.

## Same-pod multiprocess control

Schema v11 runs each client in its own process with its own endpoint, encoder,
NetEq set, packet queue, and one-thread runtime; star also uses a separate SFU
process. Cross-process latency uses `CLOCK_MONOTONIC`. The corrected matrix has
three seeds for every N/topology/layout combination. `TCP_NODELAY` is required
on the setup control sockets: without it, one delayed-ACK interval per
connection inflated N=32 direct setup to 24 seconds. Corrected setup is 603 ms
direct and 77 ms star at N=32.

| N | Topology | Layout | CPU | RSS (one heap / summed workers) | Latency p50 / p95 / p99 | Deadline misses |
|---:|:---|:---|---:|---:|---:|---:|
| 8 | direct | single | 160% | 36.6 MiB | 0.31 / 0.57 / 2.35 ms | 6.97% |
| 8 | direct | multi | 141% | 147.1 MiB | 0.35 / 2.40 / 9.55 ms | 3.40% |
| 8 | star | single | 167% | 34.1 MiB | 0.51 / 0.82 / 2.94 ms | 6.05% |
| 8 | star | multi | 150% | 160.8 MiB | 0.60 / 2.59 / 10.55 ms | 4.47% |
| 16 | direct | single | 214% | 74.4 MiB | 0.33 / 0.69 / 5.29 ms | 5.73% |
| 16 | direct | multi | 201% | 306.7 MiB | 0.34 / 2.26 / 10.16 ms | 2.35% |
| 16 | star | single | 212% | 52.6 MiB | 0.52 / 1.08 / 6.48 ms | 6.02% |
| 16 | star | multi | 201% | 309.9 MiB | 0.48 / 2.09 / 9.12 ms | 1.98% |
| 32 | direct | single | 277% | 186.1 MiB | 0.34 / 4.09 / 12.92 ms | 8.38% |
| 32 | direct | multi | 302% | 670.0 MiB | 0.34 / 5.61 / 24.81 ms | 5.36% |
| 32 | star | single | 298% | 91.2 MiB | 0.54 / 6.50 / 18.61 ms | 9.75% |
| 32 | star | multi | 280% | 606.5 MiB | 0.43 / 5.36 / 25.83 ms | 4.21% |

Process isolation roughly halves playback deadline misses, confirming that the
shared-runtime wakeup pattern was a harness artifact. It does not eliminate
whole-pod scheduling tails: low-N p95 latency is worse with many runtimes, and
N=32 p99 remains about 25 ms. Aggregate CPU stays similar. Summed RSS is not a
single-client production cost—each worker stays around 18-21 MiB—but it makes
the per-process Iroh/runtime base cost explicit. Every run again had zero
missing datagrams and zero send, SFU, or NetEq errors.

Raw results are on `gna-sim` under:

```text
/work/projects/godot-network-audio/target/voice-mesh/game-interest-v1/
/work/projects/godot-network-audio/target/voice-mesh/game-interest-v2-32/
/work/projects/godot-network-audio/target/voice-mesh/game-interest-soak/
/work/projects/godot-network-audio/target/voice-mesh/interest-stress-v1/
/work/projects/godot-network-audio/target/voice-mesh/interest-stress-soak/
/work/projects/godot-network-audio/target/voice-mesh/interest-stress-instrumented/
/work/projects/godot-network-audio/target/voice-mesh/interest-stress-parallel-senders/
/work/projects/godot-network-audio/target/voice-mesh/interest-stress-workers32/
/work/projects/godot-network-audio/target/voice-mesh/interest-stress-8p-all-interest/
/work/projects/godot-network-audio/target/voice-mesh/heaptrack/
/work/projects/godot-network-audio/target/voice-mesh/bounded-samples/
/work/projects/godot-network-audio/target/voice-mesh/media-impairment-v2/
/work/projects/godot-network-audio/target/voice-mesh/transport-netem-v1/
/work/projects/godot-network-audio/target/voice-mesh/media-timeline-v1/
/work/projects/godot-network-audio/target/voice-mesh/media-timeline-v2/
/work/projects/godot-network-audio/target/voice-mesh/recovery-netem-v3/
/work/projects/godot-network-audio/target/voice-mesh/churn-v2/
/work/projects/godot-network-audio/target/voice-mesh/churn-v3/
/work/projects/godot-network-audio/target/voice-mesh/topology-v1/
/work/projects/godot-network-audio/target/voice-mesh/process-layout-v1/
/work/projects/godot-network-audio/target/voice-mesh/process-layout-v2/
```

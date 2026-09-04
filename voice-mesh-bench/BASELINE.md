# Headless voice-mesh baseline

Date: 2026-09-03
Runner: `gna-sim`, 8-CPU quota, 20 GiB memory limit, no observed cgroup CPU
throttling during the matrix

Schema note: the original schema-v1 `neteq_concealed_percent` denominator
multiplied the already aggregated playout-callback count by the aggregate
receiver count. Those percentages are understated and should not be used for
comparison. Raw concealed-sample counts and the other reported metrics remain
valid. Schema v2 uses the actual number of samples pulled from receivers.

## Pre-upgrade baseline

The first matrix used Iroh 0.97, patched NetEq 0.8.3 (`4ec2cc8`), release mode,
and 10-second runs. Every participant maintained a direct QUIC connection to
every other participant. Talkers sent synthetic, overlapping speech/silence
cycles; each virtual listener owned its own staggered 10 ms playback task and
one NetEq instance per remote talker.

All 16 combinations of 4/8/16/32 participants, 1/2 talkers, and DTX on/off
completed with:

- zero failed sends, missing datagrams, malformed datagrams, or NetEq errors;
- full-mesh setup from about 11 ms (4 participants / 6 connections) to 0.48 s
  (32 participants / 496 connections);
- aggregate media CPU from 16.5-18.1% of one core (4 participants / one
  talker) to 159.9-170.3% (32 participants / two talkers);
- peak RSS from roughly 21 MiB to 149 MiB;
- aggregate outbound media from 0.055-0.062 Mbit/s at 4 participants / one
  talker to 1.14-1.28 Mbit/s at 32 participants / two talkers;
- median local one-way delivery below 0.84 ms in every case.

For the 32-participant/two-talker case, DTX reduced datagrams from 31,062 to
25,978 (16.4%) and aggregate media bandwidth from 1.283 to 1.138 Mbit/s
(11.3%). It did not materially change CPU in the smaller cases; at 32
participants the one samples were 170.3% without DTX and 159.9% with DTX.

The deliberately strict 2 ms callback-lateness rate rose from below 1% in most
4-participant cases to 3.6-4.8% at 32 participants/two talkers. Actual skipped
10 ms ticks were much rarer and noisy: 63 without DTX versus 345 with DTX out
of roughly 32,000 participant-ticks. Repeat runs are needed before treating
that difference as causal.

Raw results are on `gna-sim` at:

```text
/work/projects/godot-network-audio/target/voice-mesh/baseline-iroh-0.97-neteq-0.8.3/
```

## Netem path sentinel

An 8-participant/two-talker run under `tc netem delay 40ms 10ms distribution
normal` moved measured one-way latency from sub-millisecond to 40.4 ms median,
56.7 ms p95, and 73.5 ms maximum. The loopback qdisc counted 6,191 packets and
was removed afterward. This validates loopback—not the repository's old
same-namespace veth pair—as the uniform-impairment path for the next sweep.

## Iroh 1.1 smoke comparison

Iroh and `iroh-base` were then updated to 1.1. The local-only builder migration
is small: use `Endpoint::builder(presets::Minimal)` before clearing/configuring
IP transports. Two comparable 10-second DTX-on runs with NetEq still at 0.8.3
completed without transport or decoder errors.

| Participants | Iroh | Setup ms | CPU, one-core % | RSS KiB | Median latency us | Skipped listener ticks |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 0.97 | 15.6 | 31.3 | 22,092 | 355 | 0 |
| 4 | 1.1 | 12.0 | 31.2 | 22,564 | 349 | 3 |
| 32 | 0.97 | 465.5 | 159.9 | 148,912 | 786 | 345 |
| 32 | 1.1 | 516.8 | 116.7 | 134,796 | 316 | 239 |

The 32-participant result suggests materially lower CPU/RSS and local latency
on Iroh 1.1, but these are single samples in a noisy shared environment. Keep
the upgrade, then repeat the full matrix after NetEq 0.9.1 is pinned.

## Iroh 1.1 + NetEq 0.9.1 matrix

The NetEq statistics correction was rebased onto 0.9.1 and published at the
immutable fork revision `360d31c`. The same 16-case, 10-second matrix was then
rerun with Iroh 1.1 and NetEq 0.9.1. All cases again had zero failed sends,
missing or malformed datagrams, and NetEq errors. The cgroup's throttling
counters did not change during the matrix.

Across these single samples, median CPU changed by only +0.4% relative to the
pre-upgrade matrix. Median RSS changed by -2.9%, with a more consistent 9-10%
reduction in all four 32-participant cases. Median local delivery latency
changed by +1.9%. This does not reproduce the large CPU/latency improvement in
the earlier two-case Iroh 1.1 smoke, so that result should be treated as shared
runner noise rather than an upgrade effect.

| Talkers | DTX | CPU before / after, one-core % | RSS before / after, KiB | Median latency before / after, us | Deadline misses before / after |
|---:|:---:|---:|---:|---:|---:|
| 1 | off | 100.0 / 107.8 | 142,660 / 128,620 | 838 / 861 | 2.78% / 2.92% |
| 1 | on  | 84.7 / 97.3 | 142,720 / 128,208 | 755 / 810 | 2.59% / 2.99% |
| 2 | off | 170.3 / 172.1 | 148,868 / 135,300 | 789 / 831 | 3.56% / 2.50% |
| 2 | on  | 159.9 / 152.2 | 148,912 / 135,092 | 786 / 795 | 4.76% / 4.06% |

The upgraded 32-participant/two-talker DTX case sent 26,040 datagrams at 1.142
Mbit/s, used about 1.52 aggregate cores and 132 MiB RSS, and skipped 163 of
roughly 32,000 virtual-listener ticks. The no-DTX case used about 1.72 cores
and 132 MiB RSS. There was one sender scheduling skip in that run, but every
datagram which the harness attempted to send was received.

Raw upgraded results are on `gna-sim` at:

```text
/work/projects/godot-network-audio/target/voice-mesh/baseline-iroh-1.1-neteq-0.9.1/
```

The 40±10 ms netem sentinel was repeated on the upgraded stack. It counted
11,411 loopback packets and measured 40.5 ms median, 56.9 ms p95, and 75.8 ms
maximum one-way latency, with zero missing datagrams or NetEq errors. The
loopback qdisc was again removed and verified as `noqueue` afterward.

# Godotless experiment review

Date: 2026-09-04

This is the post-experiment adversarial review of the headless Iroh, Opus, and
NetEq harness. Three independent reviewers examined the audio state machine,
experimental method, and transport/system interpretation. The review included
code inspection rather than only reading the result summaries.

## Decision

The headless work clears the gate for beginning Godot/audio-thread experiments.
It supports a deliberately narrow claim: on this eight-CPU pod, Iroh 1.1 can
carry the modeled 8-32 participant game-voice traffic over direct meshes or a
static forwarding star without delivery errors, decoder errors, runaway queues,
or an endpoint-sized CPU/RSS problem. It does not establish public-Internet,
relay/NAT, perceptual-quality, or Godot mixing performance.

No additional large local-loopback matrix is required before Godot. Preserve
the harness as a regression lane, and add a small multi-host/direct-path control
later rather than extrapolating the loopback result to real paths.

## Scaling interpretation

The direct mesh has explicit quadratic connection state: 28, 120, and 496
connections at N=8, 16, and 32. Setup medians were 35, 121, and 464 ms. From
N=8 to N=32 that is a 13.3x setup increase for a 4x participant increase
(approximately N^1.86 over these points), while connection count rises 17.7x
(approximately N^2.07 because the small-N `N(N-1)/2` ratio is slightly above
16x). Setup time per connection actually falls from 1.25 to 0.94 ms.

Media CPU rose 155% to 297% of one core and RSS 36.6 to 185.5 MiB in the same
original topology matrix. Those grow much more slowly than connection count.
There is therefore no observed greater-than-quadratic cliff through N=32. The
early warning is the known O(N^2) connection/setup state itself, plus serialized
setup; the experiment neither predicts nor needs to explore behavior above the
product's current 32-participant bound.

The static star confirms the expected trade: N connections and much lower
client upload/setup/RSS, but similar aggregate CPU because encryption, forwarding,
and decode work still occur somewhere. It is a useful authoritative-server
baseline, not evidence that a star automatically reduces total compute.

## Corrections made by review

1. `VoiceEncoder` advanced RTP timestamps only when an application packet was
   emitted. DTX silence and scheduler-skipped capture frames now advance the
   media clock without consuming sequence numbers.
2. Talkspurt recovery now covers loss of the end marker, loss of both the end
   and next start markers, and reordered stale end markers. Unit tests cover all
   three cases.
3. Multiprocess workers now use a shared absolute monotonic start barrier. CPU
   is sampled at media end and excludes delivery grace in both the numerator
   and denominator.
4. Recovery `netem` changes are synchronized to an application media-ready
   marker instead of process launch.
5. The Rust NetEq fork's `silent_concealed_samples` is not a Chromium-equivalent
   signal: it marks every Expand sample silent. JSON retains the historical raw
   value but marks it invalid, summaries omit it, and the total concealed-sample
   denominator is now serialized explicitly.
6. The initial DRED report conflated decodable output with DRED coverage.
   `opus_dred_parse` reports 0/55/95 ms at 16/24/32+ kb/s; output outside that
   history is neural PLC, not recovered DRED.

Older DTX timing/concealment results and older single-versus-multiprocess CPU
ratios should not be used as final comparisons. Their connection counts, setup
times, traffic, successful-delivery counts, and bounded-resource findings remain
useful where independent of these bugs.

## Corrected controls

Three corrected N=32 seeds, eight seconds each, produced:

| Topology | Layout | Median CPU | CPU range | Median RSS | Missing / NetEq errors |
|:---|:---|---:|---:|---:|---:|
| direct | single | 315% | 307-315% | 180 MiB | 0 / 0 |
| direct | multi | 318% | 307-328% | 661 MiB summed | 0 / 0 |
| star | single | 311% | 302-320% | 86 MiB | 0 / 0 |
| star | multi | 387% | 318-391% | 600 MiB summed | 0 / 0 |

Direct single/multiprocess CPU now overlaps closely. The star multiprocess
spread is too noisy to rank and does not change the topology conclusion.
Multiprocess RSS is expectedly much higher because 32-33 processes each carry
runtime and dynamically linked-library base state.

A corrected 14-second N=8 transport control used four seconds clean, four
seconds impaired, then six seconds clean. Static 40+/-10 ms and 80+/-30 ms
jitter delivered every application datagram. The 60+/-20 ms plus 1% recovery
profile lost 141 application datagrams; the burst profile lost 261. Both held
NetEq's maximum target at 80 ms, kept buffers bounded at 139 and 120 ms, and
reported zero NetEq errors. `tc` counted nonzero shaped traffic and drops, and
the runner restored loopback to `noqueue`.

Total concealment remains a diagnostic, not an audio-quality score. It includes
receiver warm-up and intentional lifecycle effects in this synthetic workload;
the test tone also cannot establish intelligibility.

Raw corrected results are on `gna-sim` at:

```text
/work/projects/godot-network-audio/target/review-2026-09-04/
```

## Claims not supported yet

- Relay discovery, NAT traversal, migration, or geographically independent
  paths: all Iroh endpoints are local and direct.
- Independent per-route delay/loss: one loopback qdisc correlates every route.
- Queue/capacity saturation: there has not been a bandwidth/queue sweep near a
  bottleneck.
- Exact per-route ordering, duplication, or misdelivery: aggregate counts do
  not prove these invariants.
- Star availability: forwarder stall, disappearance, and recovery are untested.
- Perceptual quality: synthetic tones and concealment counters are not listening
  tests. DRED needs recorded speech if it becomes a real product candidate.

## Ranked follow-up

1. Begin the Godot scale gate: one real client should mix/spatialize the maximum
   locally audible set (start at seven sources), then exercise 16/32 connected
   peers with game-shaped interest under render/audio-thread contention.
2. Preserve the corrected N=8 recovery and N=32 direct/star cases as automated
   regression sentinels.
3. Before external user testing, run a compact two-host or multi-pod direct-path
   control with per-host CPU/RSS and route-specific sequence accounting. Add
   relay/NAT cases only when that deployment path is selected.
4. Test static-star stall/disappearance only if the authoritative-server design
   remains a candidate.
5. Defer DRED quality work. If revisited, use recorded calibration speech,
   several burst lengths/bitrates, aligned WAVs, and listening or ViSQOL; compare
   plain PLC, in-band FEC, and DRED under the same trace.


# Game-voice scenario plan

This plan extends the initial full-mesh smoke benchmark into a reusable tester
for positional or interest-managed game voice. It intentionally keeps Godot,
audio devices, acoustic processing, and spatial mixing out of the first phases.

## What existing work contributes

No surveyed project provides the complete workload needed here. The useful
pieces are complementary:

- WebRTC's network-emulation framework models independent endpoints and routes,
  capacity serialization and queues, delay, loss, packet overhead, reordering,
  and configuration changes in real or simulated time.
- WebRTC's NetEq quality tests use uniform loss, Gilbert-Elliott burst loss,
  fixed outages, and sender/receiver clock drift. Its RTP replay tool can emit
  decoded audio and aggregate statistics from captured packet histories.
- RFC 3611 separates network loss from jitter-buffer discard and reports
  burst/gap density and duration, end-system delay, and nominal/maximum jitter
  buffer delay. Current WebRTC statistics add concealment events, acceleration
  and deceleration samples, and interruptions lasting at least 150 ms.
- AOI voice-chat research models players moving through a two-dimensional
  world, dynamic circular interest regions, direct delivery, and forwarding.
  Its exact scale and codec assumptions are dated, but dynamic membership and
  the direct-versus-forwarding comparison remain relevant.
- ViSQOL provides repeatable full-reference perceptual scores for selected
  real-speech cases. It should complement, not replace, timing and continuity
  metrics, and should aggregate several fixtures per treatment.

## Workload model

The primary scenario has 32 persistently connected participants. Positions and
interest change while connections remain established. Four conversational
groups are active; each talker has 4-8 interested listeners. Talker ownership
rotates through all participants.

Generate speech activity as deterministic 10 ms timelines rather than as
independent Bernoulli talkers. Start with approximately 40% activity per
participant inside an active conversation, but suppress other full turns while
the floor is occupied. Make 20-30% of turn transitions overlap, usually for
50-200 ms, and include short backchannels. These are workload defaults, not
claims about one universal conversation distribution; seeds and parameters
must be recorded in every result.

Interest changes come from two interchangeable inputs:

1. A synthetic 2D movement model with distance-based interest and hysteresis.
2. A scripted event timeline for reproducible boundary crossings, teleports,
   group merges, and group splits.

## Direct-delivery experiments

Run the identical activity and interest timeline through these variants:

1. **Sender-filtered direct:** send only to currently interested listeners.
2. **Broadcast-and-discard:** send to every peer and discard outside interest
   at the receiver. This isolates transport fanout from decoder/playout cost.
3. **Receiver lifecycle policies:** compare eager receivers, lazy creation, and
   a warm idle timeout. Any peer must be able to start talking.

Measure transitions explicitly:

- interest-entry to first decoded and first non-concealed audio;
- interest-exit to last decoded audio, including stale-audio violations;
- talkspurt-start to first audio and talkspurt-end tail;
- sender fanout completion span and per-peer send failures;
- queue depth/delay and 10 ms playout deadline misses per participant;
- active/warm receiver counts and receiver creation/destruction latency.

Report per-participant distributions and worst participants in addition to
aggregate values. A healthy mean must not hide one overloaded listener.

## Network behavior

Use two explicitly different impairment layers:

- A transport-path layer below Iroh, initially `tc netem`, tests Iroh/QUIC loss,
  congestion, and path behavior. Extend it to independent directed routes only
  if the pod can do so cleanly with classified qdiscs or isolated processes;
  validate qdisc counters and application-observed delay every time.
- A deterministic media-boundary layer between Iroh receipt and NetEq insertion
  gives reproducible per-route NetEq tests. It must be labeled clearly because
  it does not exercise Iroh under the injected impairment.

Cross-check representative media-boundary profiles against transport-path
profiles. Do not draw Iroh robustness conclusions from media-boundary tests.

Support these primitives:

- fixed delay plus jitter, with optional reordering and duplication;
- uniform random loss;
- Gilbert-Elliott burst loss parameterized by total loss and mean burst length;
- fixed outages starting at scripted times;
- route capacity, serialization delay, queue limit, and overflow drops;
- independently controlled sender timestamp and receiver playout clock drift;
- time-varying profiles, including clean-to-impaired-to-clean transitions.

Initial treatments should include uniform loss at 1%, 3%, and 5%; burst loss at
the same totals with 30, 60, and 120 ms mean bursts; 100, 300, and 1000 ms fixed
outages; and capacity sweeps around the measured sender fanout rate. Use delay
and jitter values as a small crossed matrix rather than combining every knob.
Include a recovery scenario because steady-state summaries do not show how
quickly NetEq's target delay grows and shrinks after a path change.

For every sender/listener direction, add:

- attempted, delivered, network-lost, late-discarded, duplicate, and reordered
  packet counts;
- burst/gap loss density and duration;
- one-way transport delay and end-to-playout delay percentiles;
- NetEq actual, target, and minimum jitter-buffer delay over time;
- concealed and silent-concealed samples, concealment event count, discarded
  packets, acceleration/deceleration samples, delayed-outage events, and
  interruption count/duration;
- time to recover target delay and concealment rate after impairment ends.

## Quality lane

Keep Opus fixed for the primary scale, topology, and churn work. Perceptual
scoring is an optional diagnostic, not a release gate while the codec and NetEq
algorithm are trusted dependencies. Use it when timing/concealment metrics or
listening reveal an unexplained audible regression, or when deliberately
comparing codec/FEC choices such as experimental neural FEC.

The Linux benchmark currently dynamically links the system libopus 1.5.2; the
`audiopus_sys` fallback source is 1.3.1, so deployed versions differ by platform.
Libopus 1.4 retuned in-band FEC and DTX, 1.5 introduced DRED and Deep PLC, and
1.6 substantially improved DRED while adding speech bandwidth extension, Opus
HD/96 kHz, and a 24-bit API. Before enabling new codec behavior, compare 1.5.2
with stable 1.6.1 using identical 48 kHz mono settings, DRED disabled, and DTX
both on and off. Record the runtime version in every result. Then evaluate DRED
separately under burst loss; do not attribute a library upgrade and a feature
toggle to one treatment.

For that narrower lane, replay several 8-10-second clean speech fixtures
through representative network treatments, write each decoded stream to WAV,
and score it against the aligned clean reference with ViSQOL. Use multiple
speakers/fixtures and retain the raw WAVs for listening. Include codec CPU,
bitrate, algorithmic delay, and recovery from burst loss alongside quality.
Do not apply a single-source full-reference score to a mixed multi-speaker
output.

**1.6.1/DRED spike complete:** the focused five-run comparison found roughly
6% higher voiced encode cost, 17% higher mixed voice/silence cost, and 73%
higher continuous-silence cost in 1.6.1, with unchanged packet behavior. DRED
did not fit at 16 kb/s; at 24 kb/s and above it functionally recovered a 100 ms
scripted gap, while adding 21-180% encoder CPU and 1-15% wire bytes depending on
bitrate. See `OPUS_1_6_DRED_RESULTS.md`. Keep production on 1.5.2 pending the
broader review; recorded-speech quality work is optional follow-up rather than
a prerequisite for the Godot scale gate.

## Topology, churn, and realism sequence

1. Run clean sender-filtered and broadcast-and-discard interest workloads for
   8, 16, and 32 participants; use at least five deterministic seeds and one
   ten-minute 32-participant soak. **Complete:** all clean runs delivered every
   datagram without send or NetEq errors; sender filtering materially reduced
   traffic and CPU.
2. Compare immediate receiver retirement with bounded warm reuse. Confirm that
   current RSS plateaus under repeated interest changes before impairment.
   **Complete:** Heaptrack attributed the retained state to the simulator's 32
   Iroh endpoints, pooled Opus/NetEq receivers, and unbounded diagnostic timing
   vectors—not an allocator cache or NetEq leak. Schema v5 caps each mergeable
   percentile sample at 4,096 observations while preserving exact counts and
   maxima. A corrected five-minute soak rose from 125 MiB to 223 MiB, but growth
   tapered to about 4 MiB over its final two minutes as receiver pools reached
   their per-listener high-water marks; CPU and delivery remained clean.
3. Add a short crowd burst, group merge/split, and rapid interest-boundary
   oscillation to expose correlated scheduling stalls. **Complete:** all three
   deterministic profiles and stress-window metrics are implemented; the
   all-speaker burst is the first clean workload to produce a pronounced
   correlated deadline spike. Follow-up timing showed that the severe
   32-participant rate is predominantly a co-located-runtime scheduling effect;
   an eight-client/all-seven-speakers endpoint control stayed near its
   non-stress deadline rate. Keep aggregate transport and endpoint playout
   interpretations separate.
4. Apply selected steady, burst, outage, capacity, and recovery profiles.
   **In progress:** schema v6 implements deterministic media-boundary uniform
   loss, Gilbert-style burst loss, and clean-impaired-clean outages with explicit
   injected-drop counters, expanded NetEq lifetime metrics, and a 1 Hz recovery
   timeline. A guarded loopback-netem runner separately validates Iroh transport
   delay/loss and qdisc counters. The apparent 5% loss buffer runaway was a
   wrapper talkspurt-state bug: a lost start marker prevented playout from
   draining later voiced packets. Implicit restart on a newer nonempty packet
   bounds the reproduced run at 226 ms overall and 96 ms after startup. A
   separate fork fix now preserves transport arrival timestamps during NetEq
   delay estimation. Schema v7 then found and fixed an empty-histogram startup
   bug behind the remaining target-delay tail. Across 15 clean/static-jitter/
   recovery runs, only one 80±30 ms seed reached a 100 ms target briefly; none
   reached 150 ms, buffers remained bounded, and recovery completed without
   NetEq errors. Continue with capacity only when topology traffic requires it;
   node turnover is now the higher-value next test.
5. Join, leave, reconnect, and replace peers while unaffected conversations
   continue. Measure collateral gaps on healthy routes separately from the
   recovering participant. **In progress:** schema v8 closes and replaces live
   connection handles. Across nine eight-peer reconnect runs, all seven links
   rebuilt in 16-28 ms median with zero reconnect/NetEq errors. Affected gaps
   tracked configured downtime while the worst unaffected gap was 54 ms versus
   46 ms clean. Add new-identity join/replacement and permanent leave next.
6. Run the same timelines through a static forwarding node. Compare direct and
   forwarded end-to-playout delay, endpoint CPU/upload, forwarder CPU/traffic,
   collateral impact, and behavior when the forwarder stalls or disappears.
7. Repeat selected cases as multiple processes and across real paths. Use the
   existing WireGuard/NordVPN harness or recorded path traces to expose NAT,
   relay, socket-buffer, and host-scheduling behavior hidden by loopback.
8. Only then add the Godot audio-thread, source-count, spatial-mixing, and
   render-contention layer, preserving the same timelines and metrics.

## References

- [WebRTC Network Emulation Framework](https://webrtc.googlesource.com/src/+/HEAD/test/network/g3doc/index.md)
- [WebRTC NetEq tools](https://webrtc.googlesource.com/src.git/+/refs/heads/main/modules/audio_coding/neteq/tools/)
- [WebRTC NetEq quality-test loss models](https://webrtc.googlesource.com/src/+/10542f21c8e4e2d60b136fab45338f2b1e132dde/modules/audio_coding/neteq/tools/neteq_quality_test.cc)
- [WebRTC NetEq statistics API](https://webrtc.googlesource.com/src/+/refs/heads/main/api/neteq/neteq.h)
- [RFC 3611: RTCP Extended Reports](https://www.rfc-editor.org/rfc/rfc3611.html)
- [Peer-to-Peer AOI Voice Chatting for MMOGs](https://staff.csie.ncu.edu.tw/jrjiang/publication/Camera-readay.pdf)
- [Timing in turn-taking and its implications for processing models of language](https://pmc.ncbi.nlm.nih.gov/articles/PMC4464110/)
- [Google ViSQOL](https://github.com/google/visqol)
- [Pantheon network-emulation methodology](https://www.usenix.org/conference/atc18/presentation/yan-francis)

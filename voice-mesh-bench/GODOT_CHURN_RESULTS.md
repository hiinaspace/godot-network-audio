# Godot source/talker churn gate

Date: 2026-09-04

Runner: `gna-sim`, 12 visible CPUs with an eight-core cgroup quota, Godot
4.7.2, PulseAudio null sink, Iroh 1.1, Opus 1.5.2, patched NetEq. Raw results
remain under `target/godot-gate/churn-*` on the runner.

## Scenario and instrumentation

The deterministic 31-peer / seven-active-source gate runs five three-second
phases: group A talks, group B replaces it, silent group A identities leave and
new identities join, the replacements talk, then group C talks while the
replacements leave abruptly. A final second lets DTX end markers drain.

The harness correlates load-generator, Godot, and transport events with wall
and monotonic clocks. It measures first non-silent output, disconnect delivery,
graceful and abrupt audio tails, collateral concealment, per-frame cadence,
per-stream audio callback progress, CPU, and RSS. Full traces are staged on
pod-local `/tmp` during the measurement; writing a 28 MiB trace directly to the
shared workspace was itself able to block Godot for seconds.

Two integration changes were necessary for a meaningful test:

- Direct voice packets no longer duplicate every datagram into Godot's
  informational event queue. Draining that unbounded copy had blocked the main
  thread for about six seconds.
- `AudioStreamNetwork.deactivate()` atomically makes a disconnected stream
  silent and stops NetEq work without mutating Godot's live audio graph. The
  demo removes routing immediately and defers player/resource reclamation to a
  non-interactive point. A bounded production reclamation/pool policy is still
  required for long sessions.

The load generator also pre-binds replacement endpoints and moves connects,
disconnects, endpoint destruction, telemetry writes, and final shutdown away
from the 20 ms media clock.

## What passed

Clean runs demonstrate the intended behavior at the target scale:

- all 38 joins and 38 disconnects are observed;
- all 5,481 datagrams arrive, with zero queue drops and zero send errors;
- all 28 talker activations become non-silent;
- first output is normally about 120-175 ms;
- disconnect delivery is normally below 17 ms;
- graceful DTX tails are normally about 180-212 ms and abrupt departures stop
  within about 6 ms;
- unrelated active streams show zero collateral concealment;
- RSS is about 148-151 MiB in the debug GDExt build.

These runs support the earlier conclusion that iroh plus one NetEq lane per
audible talker can carry game-shaped voice traffic. Churn does not expose a
packet-routing, queue-growth, or identity-replacement failure.

## Unresolved gate failure

The headless Godot result is not yet repeatable. Both 3D and 2D runs
intermittently show either or both of these whole-process effects:

1. The main thread stops producing trace rows for roughly 1-5 seconds around a
   burst of seven disconnects. Step-level tracing showed the first disconnect
   callback can complete its explicit work before the gap; removing live player
   stop/detach/free operations did not eliminate it.
2. More importantly, the main thread can remain healthy while every active
   `AudioStreamNetwork` playback stops receiving mix callbacks for roughly
   0.3-1.5 seconds. Packets continue arriving every 20 ms. In a representative
   3D run, all seven players were requested at about 62 ms; three became
   non-silent at 116 ms, while four had mixed only one 128-frame block and did
   not resume until about 1.55 s. The same pattern occurred in repeated plain
   `AudioStreamPlayer` controls, so spatialization is not the cause.

Representative isolated runs:

| Mode | Seed | First output max | Disconnect max | Whole-audio callback gap | Main trace gap |
|---|---:|---:|---:|---:|---:|
| 3D | 49 | 1,554 ms | 7 ms | 1,431 ms | 52 ms |
| 3D | 50 | 145 ms | 2,173 ms | includes main stall | 2,170 ms |
| 2D | 52 | 1,508 ms | 1,262 ms | 1,396 ms at startup | 1,259 ms later |
| 2D | 53 | 410 ms | 42 ms | hundreds of ms | 62 ms |
| 2D | 54 | 1,497 ms | 8 ms | about 1.4 s | 46 ms |

CPU affinity (Godot on CPUs 0-7, fleet on 8-11) and lowering fleet priority did
not remove the pauses. One orphan receiver from an interrupted harness run was
found and removed, but clean-pod repeats still reproduced them. CPU figures are
bimodal (roughly 40-84% of one core), consistent with periods in which the
receiver is not running, and should not be used as a scale curve yet.

## Interpretation and next gate

The current evidence separates the substrate from the client integration:
iroh continues to deliver on time, but the headless Godot/PulseAudio execution
environment intermittently stops servicing the whole mixer. Because 2D and 3D
fail alike, redesigning spatial source APIs or tuning NetEq would be reacting at
the wrong layer.

Before silent-connection profiling or render contention, repeat the same gate
with the receiver and synthetic fleet in separate pods or hosts. This removes
hundreds of load-generator runtime threads and endpoint teardown from the
receiver's cgroup. Also repeat a fixed seven-talker, no-churn startup several
times: the audio callback pause already occurs before the first turnover. If it
persists with an isolated receiver, collect Godot audio-thread scheduling/futex
data and PulseAudio server underrun/latency diagnostics, or compare a supported
host audio backend. Until that boundary is stable, Godot CPU, latency, and
silent-peer overhead measurements are not trustworthy.

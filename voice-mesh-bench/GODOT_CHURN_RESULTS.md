# Godot source/talker churn gate

## Isolated-host follow-up (2026-09-05)

The dedicated `gna-loadgen` worker is operational. Receiver/audio/capture run
on `gna-sim`, the synthetic fleet runs on another physical host in `gna-loadgen`,
and cc-0 only orchestrates. The controller is `scripts/run_godot_isolated.py`:

```sh
python3 scripts/run_godot_isolated.py --fixed --spatial 3 --repeats 1
python3 scripts/run_godot_isolated.py --fixed --spatial 2 --repeats 1
python3 scripts/run_godot_isolated.py --spatial 3 --repeats 1
```

Build the debug Iroh-enabled extension on the receiver and the release
`godot_voice_churn` binary on the loadgen first. Both use the normal project
paths; loadgen's `target` points to `/build/target`. The worker pins the existing
Godot 4.7.2 executable (SHA256
`8d106cbe6144c2dc7e881d61d2429c1a8a76e6b22ef48bd5e48dcf934953f71e`),
because the default `godot` now resolves to 4.6.1. UDP uses receiver port 42000
and distinct loadgen ports starting at 42001, including replacements. The final
run explicitly recorded selected direct IP paths with no relay.

Six isolated runs reproduced the problem: three fixed-seven 3D startups, one
fixed-seven 2D startup, and two 31/7 churn runs. The first was a preliminary
control; the remaining five used callback entry/exit instrumentation. Every run
delivered all expected packets (5,327 fixed / 5,481 churn). All six recorded
zero CPU throttling on both pods. The five instrumented runs had sender deadline
lateness below 0.4 ms with no missed 20 ms deadlines.

| Instrumented case | Callback invocation gap max | Callback execution max | Receiver-local first enqueue → output max |
|---|---:|---:|---:|
| Fixed seven, 3D, repeat 1 | 1,364 ms | 5.3 ms | 1,449 ms |
| Fixed seven, 3D, repeat 2 | 1,285 ms | 5.3 ms | 1,366 ms |
| Fixed seven, 2D | 1,412 ms | 7.3 ms | 1,482 ms |
| 31/7 churn, repeat 1 | 1,097 ms | 6.6 ms | 1,186 ms |
| 31/7 churn, final | 1,087 ms | 6.8 ms | 1,168 ms |

Main-loop trace gaps stayed below 46 ms in these instrumented controls. The
callback timer measures elapsed monotonic time from one callback's exit to the
next entry; the duration timer covers the callback itself. This distinguishes a
delay outside our mixing/NetEq callback from a slow callback. It does **not**
identify the exact Godot/PulseAudio lock, buffering behavior, or scheduler cause.
The older same-pod explanations about reclamation, shared CPU quota, or trace
I/O being the root cause of all pauses were hypotheses, not established causes.

Cross-host raw wall-clock latency fields are explicitly labeled unadjusted.
Persistent SSH clock probes before/after the final run bounded sender-minus-
receiver offset to 6.338–7.814 ms; its corrected first-output maximum was
1,170.170–1,171.646 ms. The receiver-local enqueue/output measurement above
requires no host-clock synchronization. Earlier isolated runs used wider SSH
startup bounds, so use their local instrumentation for timing conclusions.
Lifetime packet/concealment counters and zero final stream counts do not prove
audible continuity: final churn had zero concealed samples despite the pause.
Retired streams remain retained until shutdown under the current demo policy;
zero registered streams is not proof of bounded long-session reclamation.

The 20–30 repeat matrix was stopped after reliable reproduction. The next
bounded diagnostic is tracing the receiver's Godot/PulseAudio waiting behavior.
`strace true` currently fails on `gna-sim` with
`PTRACE_TRACEME: Operation not permitted`. No pod privilege was changed. `time`
and `strace` were installed through the already-authorized package mechanism and
added to `/work/.bootstrap/apt-extra.txt`. See the updated org fleet handoff for
the capability check needed before resuming.

Raw artifacts remain on `gna-sim` under `target/godot-gate/isolated/`; the final
fully versioned run is `churn-3d-1788574914747959375-0` at source `637111d`.
Its manifest includes engine/extension hashes, clean source state, PulseAudio
information, both cgroups' CPU/pressure snapshots, and clock bounds. Source and
test binaries have been synchronized across both workers. No network/audio
load ran on cc-0.

## Original same-pod experiment

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

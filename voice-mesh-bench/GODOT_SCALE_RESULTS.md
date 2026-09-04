# Godot scale gate

Date: 2026-09-04

Runner: `gna-sim`, 8-CPU quota, 20 GiB memory limit, Godot 4.7.2, PulseAudio
null sink, Iroh 1.1, Opus 1.5.2, patched NetEq. Raw results remain on the runner
under `target/godot-gate/`.

## What changed before measuring

The old GDExt surface discarded remote identity and effectively exposed one
receive stream. It was replaced rather than preserved:

- `IrohVoiceTransport` now exposes one stable `AudioStreamNetwork` per endpoint
  ID, with a bounded preregistration packet queue.
- Each stream is assigned to its own `AudioStreamPlayer` or
  `AudioStreamPlayer3D`; Godot owns mixing and spatialization.
- One local encoder still fans identical packets out to every peer by default.
  `set_send_peers` supplies interest-managed fan-out without per-peer encoders.
- Disconnect stops/removes the peer player and receive stream. This prevents
  endless NetEq PLC after a real departure.
- Transport arrival times are translated from the sidecar's monotonic epoch to
  each stream's clock domain. NetEq remains owned by the audio thread.

The gate also found and fixed several harness/configuration problems: portable
Godot/Cargo discovery, explicit headless PulseAudio selection, a forced
synthetic-input sample-rate mismatch, synthetic catch-up bursts after blocking
connect, and the need for a current `Camera3D` for audible headless 3D mixing.
The four-packet startup gate was retained: starting a player on one packet made
Godot pull NetEq continuously before useful input existed and generated large
startup concealment.

## Results

All CPU figures are the receiver process's CPU time divided by wall time, as a
percentage of one core. RSS is receiver maximum resident memory. Rust-load
runs keep the receiver alive for three seconds after media so disconnect
cleanup is included; their packet count is exactly seven sources × 601 frames.

| Population | Source processes | Active spatial sources | Receiver CPU | Max RSS | Packets | Conceal/drop/errors | Output |
|---|---|---:|---:|---:|---:|---|---|
| 1 | Godot | 1 | 9.4% | 138.8 MiB | 241 | 0 / 0 / 0 | -24.9 dBFS |
| 7 | Godot | 7 | 25.5% | 139.5 MiB | 3,428 | 0 / 0 / 0 | -14.5 dBFS |
| 31 | Godot | 0 | 24.8% | 140.3 MiB | 0 | 0 / 0 / 0 | silent |
| 31 | Godot | 7 | 42.3% | 142.4 MiB | 3,791 | 0 / 0 / 0 | -14.1 dBFS |
| 7 | lightweight Rust | 7 | 20.8% | 139.9 MiB | 4,207 | 0 / 0 / 0 | -14.4 dBFS |
| 31 | lightweight Rust | 0 | 29.0% | 140.3 MiB | 0 | 0 / 0 / 0 | silent |
| 31 | lightweight Rust | 7 | 39.7% | 142.6 MiB | 4,207 | 0 / 0 / 0 | -14.4 dBFS |

The 31-peer Rust-load run reached all 31 connections in about 192 ms. Its
receiver frame delta was 6.94 ms at p99 with a 43.75 ms maximum. At media end,
all streams were removed and the final transport state was zero peers and zero
receive streams. This is the cleanest current Godot receiver result because it
does not co-locate 31 additional Godot runtimes.

The seven-active delta within the 31-peer Rust population is about 10.7% of one
core and 2.3 MiB of RSS. Conversely, the 31-connected/zero-active case still
uses 29% of one core. Silent connected Iroh sessions and their integration are
therefore a more material scale cost than these seven Opus/NetEq/spatial mixer
lanes in this headless setup. That remains practical at the target population,
but is worth profiling before extrapolating.

The reported maximum NetEq buffer in Rust-load runs contains a one-sample
startup observation (up to 1,560 ms) immediately before the first audio-thread
statistics update. It collapses to roughly 30-70 ms on the next pulls, target
delay stays at 80 ms, the WAV has no leading silence at a -45 dB threshold,
and there is no concealment. Treat that maximum as an instrumentation/startup
artifact pending a phase-aware metric, not evidence of sustained playout
latency.

## Interpretation and next gate

This is enough to keep the current architecture: one encoder per local user,
one receive/NetEq lane per audible remote talker, and Godot-native per-source
mixing. There is no evidence here that a custom native mixer or a return to a
single mixed extension stream is needed.

The first source-churn gate is now implemented; see
`GODOT_CHURN_RESULTS.md`. It found repeatable whole-mixer pauses in both 2D and
3D headless PulseAudio controls, including cases where the Godot main loop and
iroh delivery remain healthy. Isolate the receiver from the synthetic fleet
before adding render contention or treating Godot CPU/latency figures as a
capacity result.

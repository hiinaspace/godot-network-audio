# godot-network-audio — current plan

Updated: 2026-09-09. Architecture: `DESIGN.md`. Experimental evidence:
`voice-mesh-bench/GODOTLESS_REVIEW.md`, `GODOT_SCALE_RESULTS.md`, and
`GODOT_CHURN_RESULTS.md`, and `GODOT_PULSE_WAIT_RESULTS.md` (newest).

## Target and scope

The library targets roughly 16 participants in a larger multiplayer game.
32 participants is a scaling characterization point, not a promised practical
P2P target. Initial manual testing is the user plus one or two friends.
Keep voice-core sans-IO, NetEq owned by the playback/audio thread, and Godot
mixing/spatialization per remote source. Transport ingress queues packets with
monotonic arrival metadata. One local encoder fans out to interested peers.

## Current evidence

- Iroh 1.1, patched NetEq 0.9.1 (`69c6ccb`), Godot 4.7 API / tested 4.7.2.
- Direct/interest/static-star, impairment, churn, memory, and process-isolation
  experiments are implemented. The corrected Godotless gate passed.
- Opus 1.6/DRED was evaluated; current production experiments retain 1.5.2.
  DRED is deferred; see `voice-mesh-bench/OPUS_1_6_DRED_RESULTS.md`.
- Godot supports peer-routed streams and seven simultaneous spatial sources.
  The 1.1–1.4 second startup callback gap was traced to PulseAudio null-sink
  startup latency. With `--norewinds`, small fixed/churn controls show 17–28 ms
  maximum gaps. See `GODOT_PULSE_WAIT_RESULTS.md`; earlier measurements using
  the default sink remain qualified, not a hardware or capacity result.
- Disconnect atomically deactivates playback; the example retains retired
  players until shutdown. A bounded long-session reclamation policy is pending.

## Completed: bounded mixer-pause investigation

The receiver polls normally while the default PulseAudio null sink drains
nearly two seconds of startup latency. `norewinds=1` on the temporary sink
removes that backlog. Fixed 2D/3D, traced/untraced, and 31/7 churn controls
no longer exhibit the second-long pause. No extension or Godot change was
needed. See `voice-mesh-bench/GODOT_PULSE_WAIT_RESULTS.md` for results/limits.

Run receiver/audio on gna-sim and loadgen on gna-loadgen; cc-0 only orchestrates.
Tracing and loadgen build were reverified September 9. The harness now supports
`--trace` and `--norewinds`; use the latter for normal pod controls, omit it to
reproduce the default-sink artifact. Resolve peer DNS at run start.

```sh
python3 scripts/run_godot_isolated.py --fixed --spatial 3 --norewinds
python3 scripts/run_godot_isolated.py --spatial 3 --norewinds
```

Fleet details: `~/org/godot-network-audio-fleet-infra-handoff.md`.

## Next

1. Implement bounded receive-player reclamation and validate it with the
   corrected small churn control before claiming long-session readiness.
2. Run targeted ~16-participant render/main-thread contention and silent-peer
   profiling only where it answers an unresolved constraint.
3. Move to a real desktop 2–3-person voice integration. A compelling theater
   or minigame is the route to eventual ~16-human testing; further synthetic
   work should support this rather than become an indefinite prerequisite.
4. Clean up the generic sender egress API and packaging as integration needs
   become concrete. Preserve small corrected regression sentinels.

Relay/NAT, perceptual speech quality, hardware capture/playback, and VR/render
load are not established by pod tests. No new large topology or codec sweep is
queued. Historical implementation plans remain available in git history.

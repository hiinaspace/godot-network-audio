# godot-network-audio — tactical plan

This file tracks current status and the next implementation milestones.
Durable architecture notes live in `DESIGN.md`.

## Current status

Implemented and working at a prototype level:
- `voice-core` crate with Opus encode/decode, DTX, packet format, NetEq-backed receive path, and basic resampling helpers.
- `gdext` crate with:
  - `NetworkAudioSender`
  - `AudioStreamNetwork`
  - `AudioStreamPlaybackResampled`-based receive playback
- direct Rust loopback path used by the demo/harness
- Godot demo project under `example/`
- PipeWire-based harness that can:
  - feed a WAV into a virtual mic
  - capture Godot output
  - emit JSONL traces
  - plot stats and input/output spectrograms
- local `neteq` override for more trustworthy receiver stats while we validate behavior

Known-good behavior so far:
- local loopback path can run cleanly for long durations
- sender pacing is no longer tied to `process()` for encoding cadence
- startup prebuffer and talkspurt-resume prebuffer removed the main concealment glitches we were seeing in perfect local runs

Open gaps:
- transport integration is still demo-local; there is no realistic networked example yet
- the public sender egress path still assumes the game drains emitted packets on the main thread
- docs are ahead of some implementation details and behind others unless kept in sync intentionally
- the PipeWire harness is good enough for regression checks but still worth hardening further as we add multi-process tests

## Next milestone

### M1: optional iroh transport integration

Goal:
- add a realistic non-HLMP transport path for the extension's first real multiplayer target: small full-mesh p2p voice over iroh/QUIC datagrams

Deliverables:
- an optional iroh-facing crate or addon surface, separate from the transport-agnostic core
- one iroh `Endpoint` per local process, with a dedicated voice ALPN
- one sender-side shared local voice pipeline fan-out to N peer connections
- one receive pipeline per remote talker
- one QUIC `Connection` per remote peer, reused for bidirectional voice datagrams
- explicit packet arrival timestamps passed into `AudioStreamNetwork.push_packet_with_meta(...)`
- a minimal example with two Godot processes on one machine
- initial integration may use the current sender egress path; M3 is where that boundary gets cleaned up for non-main-thread transport consumers

Non-goals for this milestone:
- HLMP convenience API
- transport-agnostic trait abstraction for every possible backend
- congestion-control feedback beyond exposing iroh/QUIC stats for future tuning

Acceptance:
- two Godot instances can exchange voice packets over iroh datagrams
- the receiver path stays clean under local no-impairment runs
- the API boundary stays transport-agnostic in `voice-core` / base `gdext`

## Follow-on milestones

### M2: network impairment validation

Goal:
- exercise the real transport path under `tc netem`

Deliverables:
- harness support for two-process runs with transport enabled
- scripted `netem` profiles for delay, jitter, loss, reorder, burst loss
- plots and audio captures for sender/receiver behavior under those profiles

Acceptance:
- mild and moderate profiles remain intelligible and free of pathological buffer behavior
- stats and output captures are stable enough to use as regression checks

### M3: sender egress API cleanup

Goal:
- make the transport handoff cleaner than the current demo-oriented loopback/signal split

Deliverables:
- a clear packet-source API from Rust suitable for non-main-thread transport consumers
- separation between:
  - local loopback/testing hooks
  - generic outbound packet stream
  - optional transport integrations like iroh

Acceptance:
- a game or sidecar transport does not need to depend on `process()` just to move packets out of the sender

### M4: doc and packaging cleanup

Goal:
- make the repo understandable to future maintainers and users

Deliverables:
- `DESIGN.md` stays architectural
- `PLAN.md` stays tactical
- README explains:
  - core transport-agnostic API
  - optional transport integrations
  - current caveats
- decide whether loopback/testing helpers stay in the main addon or move behind features/examples

## Parking lot

Useful but not next:
- HLMP integration example, with explicit caveats about polling/threading
- Steam Audio integration/fork pass
- output-side/runtime stats polish
- optional per-peer sender adaptation lanes for bitrate/FEC
- better harness lifecycle management for long-running multi-process runs

# godot-network-audio — v0 plan

A Godot 4.6+ GDExtension that turns a microphone into voice packets and voice packets into a spatializable audio stream. **Networking is deliberately out of scope** — the extension emits `PackedByteArray`s on one side and consumes them on the other; how they travel is the game's problem (HLMP unreliable RPCs, iroh datagrams, LiveKit, a loopback signal, whatever).

Reference discussion and scoping notes are in `notes.md`. This file is the concrete plan.

## Goals / non-goals

**Goals (v0):**
- Mic → opus packets, with energy-based VAD and PTT.
- Opus packets → `AudioStream` with adaptive jitter buffer, packet-loss concealment, reordering.
- Fits into Godot's audio graph so `AudioStreamPlayer3D` spatialization "just works" per talker.
- Linux-first, but the toolchain (godot-rust) is cross-platform — other platforms come along for free modulo opus build quirks.
- A runnable loopback example in `example/`.

**Non-goals (v0):**
- Networking integration of any kind (no HLMP helper, no iroh bridge, no LiveKit, no WebRTC).
- Multi-talker mixing as a single node (each remote talker = one `AudioStreamNetwork` + one `AudioStreamPlayer`; the bus graph mixes).
- Congestion control / RTCP-style feedback (rely on neteq's adaptive target delay on receive, fixed bitrate on send).
- Mute/deafen/tag routing / team channels / per-peer ACLs.
- rnnoise (feature-gated, default off — may or may not land in v0 depending on how painful the build is).
- Convenience "drop one node and it works" wrapper. Low-level API only; we'll design the wrapper after we learn how HLMP/iroh integration actually feels.
- WebRTC wire compatibility. Our packet format is our own.

## Why this shape

From `notes.md`: the interesting, hard work is a sans-IO "voice to/from packets" state machine (encode + VAD + PLC + jitter buffer). Networking has multiple valid answers (client/server HLMP, iroh p2p, SFU) and shouldn't be baked in. So: build the sans-IO core cleanly, expose two thin Godot nodes around it, leave networking as user code + examples.

## Ecosystem survey (what we're reusing and why)

**[videocall-rs/neteq](https://github.com/security-union/videocall-rs/tree/main/neteq)** — pure-Rust port of WebRTC NetEQ. Adaptive jitter buffer, accelerate / preemptive-expand / classical-expand PLC, delay manager, stats. Sans-IO: `insert_packet(AudioPacket)` + `get_audio()` tick loop. This is the receive-side core we'd otherwise spend months reimplementing.

Battle-testedness assessment (as of 2026-04-11):
- Real production user: ships in videocall.rs itself (Security Union's commercial product).
- 68 unit tests pass in 0.23s. Coverage targets the hairy paths: late-joining peers, expand safety valve, recovery after reset, continuous-streaming convergence, reordering/jitter, escalating delays, decision-type selection.
- Zero `unsafe` in the logic; two `unsafe impl Send` markers in `time_stretch.rs` are trait-object pragmas only.
- Core dependencies are tiny: `thiserror`, `serde`, `log`, `ringbuf`, `web-time`. `AudioDecoder` is a trait — we provide our own opus decoder impl and never enable their `native` feature, so no `cpal`/`tokio`/`opus` pull-in from neteq.
- **Risks**: bus factor of 1 (Dario Lencina wrote 43 of 49 commits, ~88%). Time-stretch paths still moving — recent history has a "Critical Bug Fixes" overhaul (Oct 2025), a "bug: Fix neteq buffering" (Aug 2025), and a Feb 2026 revert-and-retry of expand improvements. `.unwrap()` density in non-test code is higher than ideal; `get_audio()` has internal unwraps assuming invariants hold.
- **Mitigation**: pin an exact version in `Cargo.toml`. `VoiceReceiver::pull_frame` wraps `get_audio()` and returns silence on `Err` rather than propagating. If upstream stalls we vendor the ~6500 LOC into our repo — the core is self-contained enough to maintain.
- **Early stress test**: before committing to the plan, build a bursty-loss + jitter-spike loopback test using `neteq/examples/basic_usage.rs` as a template and listen to the output. If it glitches badly on realistic game-network profiles, reassess before milestone 3.

**[two-voip-godot-4](https://github.com/goatchurchprime/two-voip-godot-4)** (cloned at `~/code/two-voip-godot-4`) — C++ GDExtension. Further along than `notes.md` initially suggested:
- `AudioEffectOpusChunked` (`src/audio_effect_opus_chunked.h:107`): mic bus effect with ring-buffered chunks, resampling, optional rnnoise, `chunk_max()` VAD hint, undrop-chunk for pre-clip avoidance, FEC-aware encoding.
- `AudioStreamOpusChunked` (`src/audio_stream_opus_chunked.h:77`): `AudioStream` with `push_opus_packet(bytes, begin, decode_fec)` and `chunk_space_available()`. Dumb ring buffer — no adaptive delay, no reorder, no PLC beyond opus FEC.
- Gaps: no jitter buffer, no explicit VAD gating, no RTP-ish packet metadata, no networking.
- **Decision**: greenfield Rust, don't fork. Cross-reference twovoip when we hit mic sample-rate quirks, especially the 44.1/48 mismatch and `AudioServer.get_input_frames()` usage (added in Godot 4.6 specifically to fix mic reliability — twovoip's README calls this out). Also read `audio_stream_opus_chunked.cpp`'s `_mix_resampled` for the custom `AudioStream` path, but do not copy its shared ring-buffer threading model for receive.

**[godot-iroh](https://github.com/tipragot/godot-iroh)** (cloned at `~/lib/godot-iroh`) — already a Rust gdext using godot-rust with a tokio runtime singleton pattern. Proves the toolchain path. Not a v0 dependency, but validates the approach and is the obvious target for a v0.1+ networking example.

**[univoice](https://github.com/adrenak/univoice)** — Unity, interface-based (`IAudioInput`/`IAudioOutput`/`IAudioClient`/`IAudioFilter`) with tag-based routing and mute/deafen. Reference only; their surface is a good roadmap for v0.1+ multi-peer features, but its abstractions are deeper than we need for v0.

## Design decisions (locked)

1. **Rust + godot-rust (gdext)**, not C++. Reasons: neteq slots in trivially; no cmake wrangling; matches godot-iroh's toolchain; godot-rust has stabilized enough for AudioStream subclassing.
2. **Single talker per `AudioStreamNetwork`**. Multiple concurrent remote talkers = multiple nodes = audio bus mixing. Matches spatialized-per-avatar gameplay.
3. **Adaptive target delay + fixed bitrate** is enough for v0. Skip feedback CC. Revisit if real game-network profiles break it.
4. **48 kHz, 20 ms frames, mono, ~16 kbps** defaults. Standard voice-opus config; matches neteq example assumptions.
5. **Godot 4.6+** target. Leverages `AudioServer.get_input_frames()` for mic reliability.
6. **Our own minimal packet format**, not RTP. Start/end-of-talkspurt flags that RTP doesn't carry; no padding/CSRC baggage; no interop ambition. Internally we synthesize a `neteq::RtpHeader` with fixed SSRC/PT at the boundary before handing to NetEq.
7. **`VoiceReceiver::pull_frame` never errors to callers.** Any internal NetEq failure → silence + logged stat. Isolates Godot playback from neteq's unwrap density.
8. **Sender is a `Node`** that reads microphone input via `AudioServer.get_input_frames()`, not via `AudioEffectCapture`. This follows Godot 4.6's more direct input path, avoids bus-capture drift quirks, and keeps encode off the audio thread.
9. **The audio thread owns NetEq.** `push_packet()` only enqueues packet metadata into a lock-free queue; `_mix` drains that queue and calls NetEq. No mutex shared between the main thread and the audio callback.
10. **Packet ingress carries explicit arrival timing.** The core API accepts packet arrival time as metadata so NetEq's jitter estimate reflects transport arrival, not Godot main-thread scheduling jitter.
11. **Backlog behavior is bounded and drop-oriented.** The packet queue is bounded; overflow drops stale voice packets instead of letting `_mix` inherit unbounded catch-up work.
12. **Single-talker invariant is enforced, not just documented.** `AudioStreamNetwork` owns one stream identity / sequence space and must reject or reset on mismatched stream identity rather than accidentally multiplexing.
13. **Receive audio stays mono until spatialization.** `AudioStreamNetwork` should present a mono voice source; stereoization belongs to Godot or Steam Audio, not the voice transport layer.

## Repo layout

```
godot-network-audio/
├── Cargo.toml                      # workspace
├── PLAN.md                         # this file
├── notes.md                        # original scoping notes
├── voice-core/                     # sans-IO rust crate, no godot dep
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── encoder.rs              # VoiceEncoder: pcm → opus + vad
│       ├── decoder.rs              # OpusAudioDecoder: impls neteq::AudioDecoder
│       ├── receiver.rs             # VoiceReceiver: wraps NetEq
│       ├── packet.rs               # VoicePacket wire format
│       ├── vad.rs                  # energy-based VAD + hangover
│       └── resample.rs             # input-rate → 48k
├── gdext/                          # godot-rust crate producing the .so/.dll/.dylib
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                  # ExtensionLibrary entry point
│       ├── sender.rs               # NetworkAudioSender
│       ├── stream.rs               # AudioStreamNetwork + playback
│       └── packet_bytes.rs         # PackedByteArray <-> VoicePacket
├── addons/godot_network_audio/
│   ├── godot_network_audio.gdextension
│   └── bin/                        # built libs per platform
├── example/                        # minimal godot project, loopback demo
│   ├── project.godot
│   └── loopback.tscn
└── README.md
```

## voice-core API sketch

```rust
// packet.rs — 8-byte header + opus payload on the wire
pub struct VoicePacket {
    pub seq: u16,          // wraps
    pub timestamp: u32,    // samples at 48 kHz
    pub flags: PacketFlags,// bit 0: start_of_talkspurt
                           // bit 1: end_of_talkspurt
                           // bit 2: fec_included
    pub payload: Vec<u8>,  // opus bytes
}
impl VoicePacket {
    pub fn encode_to_bytes(&self, buf: &mut Vec<u8>);
    pub fn decode_from_bytes(bytes: &[u8]) -> Result<Self, DecodeError>;
}

pub struct PacketArrival {
    pub received_at_mono_us: u64,
}

// encoder.rs
pub struct VoiceEncoderConfig {
    pub input_sample_rate: u32,     // from godot, e.g. 44100 or 48000
    pub frame_duration_ms: u32,     // 20
    pub bitrate_bps: i32,           // 16000
    pub vad: VadConfig,             // threshold, hangover frames
    pub denoise: bool,              // v0 may stub
}

pub struct VoiceEncoder { /* opus encoder + resampler + vad state */ }

impl VoiceEncoder {
    pub fn new(config: VoiceEncoderConfig) -> Result<Self>;
    /// Push interleaved f32 microphone PCM from Godot.
    /// Any fractional resampler remainder stays in encoder-owned state.
    pub fn push_pcm(&mut self, samples: &[f32]);
    /// Pull ready packets. None when under a full frame or VAD is silent.
    /// Emits start/end-of-talkspurt flags at talkspurt boundaries.
    pub fn poll_packet(&mut self) -> Option<VoicePacket>;
    pub fn set_force_transmit(&mut self, on: bool); // PTT override
    pub fn flush(&mut self); // reset opus encoder state on gap
}

// decoder.rs
pub struct OpusAudioDecoder { /* audiopus::Decoder */ }
impl neteq::codec::AudioDecoder for OpusAudioDecoder { /* ... */ }

// receiver.rs
pub struct VoiceReceiver {
    inner: neteq::NetEq,
    out_rate: u32,
    consecutive_failures: u32,
    sticky_error: Option<String>,
}
impl VoiceReceiver {
    pub fn new(sample_rate: u32) -> Result<Self>; // registers OpusAudioDecoder for PT 96
    pub fn push_packet(&mut self, pkt: VoicePacket, arrival: PacketArrival);
    /// One 10 ms frame. NEVER errors — silence on internal failure.
    pub fn pull_frame(&mut self, out: &mut [f32]);
    pub fn stats(&self) -> ReceiverStats; // godot-friendly subset of NetEqStats
}
```

Detail: on VAD off→on transition we emit a packet with `start_of_talkspurt=1` and `opus_encoder_ctl(OPUS_RESET_STATE)`. On on→off we emit one final packet with `end_of_talkspurt=1` (lets the receiver short-circuit long expand sequences).

## gdext node API sketch

```rust
#[derive(GodotClass)]
#[class(base=Node)]
pub struct NetworkAudioSender {
    base: Base<Node>,
    #[export] bitrate: i32,             // 16000
    #[export] vad_threshold_db: f32,    // -45
    #[export] push_to_talk: bool,       // bypass VAD when true
    #[export] denoise: bool,
    encoder: Option<VoiceEncoder>,
}

#[godot_api]
impl INode for NetworkAudioSender {
    fn process(&mut self, _delta: f64) {
        // 1. Drain AudioServer.get_input_frames() → encoder.push_pcm
        // 2. while let Some(pkt) = encoder.poll_packet() → emit signal
    }
}

#[godot_api]
impl NetworkAudioSender {
    #[signal] fn packet_ready(bytes: PackedByteArray);
    #[func] fn is_speaking(&self) -> bool { /* ... */ }
}

#[derive(GodotClass)]
#[class(base=AudioStream)]
pub struct AudioStreamNetwork {
    base: Base<AudioStream>,
    incoming_packets: Arc<PacketQueue>,
    stats: Arc<ReceiverStatsSnapshot>,
}

#[godot_api]
impl AudioStreamNetwork {
    #[func] fn push_packet(&mut self, bytes: PackedByteArray) { /* stamps now, then enqueue */ }
    #[func] fn push_packet_with_meta(&mut self, bytes: PackedByteArray, received_at_mono_us: int) { /* enqueue */ }
    #[func] fn get_buffer_ms(&self) -> i32 { /* ... */ }
    #[func] fn get_stats(&self) -> Dictionary { /* ... */ }
}

// AudioStreamNetworkPlayback owns VoiceReceiver privately.
// _mix drains PacketQueue up to a fixed budget, feeds NetEq, resamples to the
// current Godot mix rate if needed, and writes dst frames.
// Main/network threads never lock or call into NetEq directly.
```

### Framing detail

Godot's `AudioStreamPlayback::_mix` is called with a frame count from the audio driver — you must fill exactly that many, no preferred chunk size. NetEq produces fixed 10 ms frames (480 samples at 48 kHz). Standard pattern: the playback class keeps a small leftover buffer, each `_mix` drains leftover first, then pulls 10 ms frames until `dst` is full, stashes the tail. `two-voip-godot-4`'s `AudioStreamPlaybackOpusChunked::_mix_resampled` does exactly this — read it when implementing ours.

On output: NetEq runs at 48 kHz internally, but Godot's driver mix rate may differ. The playback object therefore needs an explicit output-side resampling step unless the stream sampling rate is guaranteed to match the driver. Make this resampler part of the playback implementation, not an accidental property of the project audio settings.

On input: `AudioServer.get_input_frames(frames)` returns microphone frames from the engine's input buffer. Use `get_input_frames_available()` / `get_input_buffer_length_frames()` to size fetch cadence conservatively, then resample to 48 kHz inside `VoiceEncoder`, accumulate to 960-sample frames, encode.

### Threading detail

Do not share `VoiceReceiver` behind a mutex between `push_packet()` and `_mix`. Godot's audio callback path should not block on the main thread. The intended shape is:

- Main thread or network thread decodes `PackedByteArray` into `VoicePacket` metadata and enqueues `(VoicePacket, PacketArrival)` into an SPSC queue.
- Audio playback object owns `VoiceReceiver` and drains that queue inside `_mix`, but only up to a fixed per-call budget.
- `get_stats()` reads atomics or a snapshot updated by the playback object, not the live receiver state behind a lock.
- Queue capacity is fixed. On overflow, the producer drops stale voice packets rather than growing latency or stalling the audio callback.

This keeps engine scheduling jitter out of the audio callback and out of NetEq's packet-arrival model.

## Future transport hooks

The core remains transport-agnostic, but the ingress API should preserve transport timing information:

- `push_packet(bytes)` is convenience API: it stamps `received_at_mono_us = now` at call time.
- `push_packet_with_meta(bytes, received_at_mono_us)` is the preferred API for real networking integrations so arrival timing survives batching on the Godot side.
- `received_at_mono_us` is always monotonic microseconds in the receiver's local clock domain, never wall-clock time.

Leave sender adaptation out of v0, but reserve a small transport-hints surface for v0.1+:

```rust
pub struct SenderTransportHints {
    pub rtt_ms: Option<u32>,
    pub datagram_send_buffer_space: Option<usize>,
    pub max_datagram_size: Option<usize>,
}
```

These hints are for sender bitrate / FEC / DTX policy only. They do not replace NetEq and should not be fed directly into the receiver jitter buffer. `max_datagram_size` should eventually constrain the sender's maximum packet payload size directly, even if the default Opus settings stay far below typical QUIC datagram MTUs.

## QUIC / iroh notes

QUIC datagrams are a good fit for voice packets, but they do not replace the receive-side jitter buffer:

- QUIC DATAGRAM gives unreliable, unordered delivery.
- QUIC still does congestion control and path MTU management.
- QUIC does not do playout timing, jitter buffering, packet loss concealment, or audio-specific delay management.

Therefore:

- Keep NetEq (or equivalent) on receive even over iroh/QUIC.
- Prefer dropping stale voice packets over waiting for datagram buffer space under congestion.
- Treat transport RTT / datagram-buffer / MTU signals as sender adaptation hints, not receiver jitter-buffer inputs.

For iroh specifically, the likely v0.1 path is:

- Use unreliable datagrams for voice packets.
- Expose optional arrival timestamps from the iroh receive task into `push_packet_with_meta`.
- Later add an iroh-specific helper/example that reads `rtt()`, `datagram_send_buffer_space()`, and `max_datagram_size()` for sender policy.

## Automated evaluation

We do not need a giant media-lab harness for v0, but we do want better regression checks than "listen to yourself once." Use a two-layer approach:

- **Small implementation-time harness**: deterministic sans-IO test that feeds recorded speech through encoder → impairment model → receiver, then checks both output audio and NetEq stats. This is intended for coding agents and CI.
- **Heavier evaluation sweep**: offline corpus run with broader impairment profiles, objective speech metrics, wav dumps, and a small human-listening canary set. This is for milestone validation, not every edit.

### Small implementation-time harness

This should land early, alongside the basic sine test, and should be cheap enough to run in normal development:

- Input: a short checked-in mono speech clip, around 5 to 10 seconds, 48 kHz wav.
- Impairments: deterministic seeded jitter, random loss, and short burst loss. Start with one "mild WAN" profile:
  - 20 ms base one-way jitter window
  - 2% random packet loss
  - one short burst loss event of 3 to 5 packets
- Output:
  - dumped wav for manual spot-checks when the test fails locally
  - JSON or struct summary of `NetEqStats`
- Assertions:
  - no panics / no receiver hard failure
  - bounded concealment and expand rates
  - bounded target-delay growth
  - output length stays within a sane tolerance of expected playout length
  - objective score above a floor once we add one

Keep this harness transport-free. It should inject impairments by transforming `(VoicePacket, PacketArrival)` events directly, because that is faster, deterministic, and avoids needing sockets just to validate NetEq integration.

### Heavier evaluation sweep

Once the basic loopback path works, add a separate offline tool or test-only binary that runs a small corpus of speech clips through a matrix of impairment profiles:

- clean
- jitter only
- mild random loss
- moderate random loss
- burst loss
- reorder + jitter
- bandwidth clamp / stale-packet-drop scenario

For scoring:

- First use `NetEqStats` counters as the cheap always-on guardrails.
- Add one lightweight objective intelligibility / quality metric next. STOI is a practical first choice.
- Add ViSQOL later if the extra dependency/build complexity feels acceptable; it is a better speech-quality metric for codec / VoIP style degradation than RMS-style comparisons.
- Optional later backstop: ASR word error rate on fixed phrases using local `whisper.cpp`. Useful, but secondary to the simpler metrics above.

The important point is not to chase perfect perceptual scoring in v0. The goal is a repeatable "this change made voice materially worse under realistic impairments" signal.

### Real network emulation

For end-to-end transport testing later, use Linux `tc netem` first. It already covers delay, jitter, loss, reorder, duplication, corruption, and rate shaping well enough for voice-path validation. If we eventually want trace-driven or mobile-network replay, Mahimahi is the next tool to consider, but it is not required for the first useful test suite.

## Steam Audio notes

The current plan is compatible with `godot-steam-audio` because that plugin wraps an inner `AudioStream` and spatializes whatever the inner stream produces. `AudioStreamNetwork` should therefore slot in as the inner stream for a `SteamAudioPlayer`.

Implementation guidance:

- Keep the voice library output mono-first. That matches positional voice as a point source and keeps the transport / jitter-buffer layer independent of any specific spatializer.
- Treat Steam Audio as a downstream spatialization layer, not as part of the receive pipeline.
- Prefer a 48 kHz project mix rate for the prototype to minimize sample-rate mismatches between Opus / NetEq and the plugin.

Known integration concerns from the local plugin code:

- `SteamAudioStreamPlayback::_mix` takes a lock on the audio callback path.
- The plugin currently uses stereo intermediate buffers in places where a mono voice-source path would be cleaner.
- The plugin derives its Steam Audio config from project settings rather than querying runtime mix settings directly.

None of that blocks v0, but it does make an early fork or patch series reasonable for the prototype goal of "Steam Audio spatialized voice chat over p2p in Godot on Linux/Windows."

## Milestones

1. **Skeleton**: workspace, `voice-core` compiles with `neteq = "=0.8.3"` (exact pin) and `audiopus`, plus `OpusAudioDecoder` impl. Unit test: encode 1 s sine → decode through receiver → assert RMS within tolerance of input.
2. **Sans-IO loopback stress test**: round-trip recorded speech through encoder → deterministic impairment harness → receiver. Keep one cheap seeded profile in normal test runs, dump output wav on failure, and record `NetEqStats`. Manual listening is still useful here, but not the only gate.
3. **gdext scaffolding**: godot-rust crate builds a .so, registers `NetworkAudioSender` and `AudioStreamNetwork`, installs into `addons/`. Verify the pinned neteq version still builds cleanly on the current Rust toolchain before doing integration work on top of it.
4. **Mic wire-up**: sender node pulls from `AudioServer.get_input_frames()` and emits `packet_ready`. Linux verified.
5. **Playback wire-up**: `AudioStreamNetwork._mix` drains the packet queue with a bounded budget, pulls from its private `VoiceReceiver`, and resamples to the driver mix rate before filling `dst`. Loopback demo scene: one `NetworkAudioSender`, `packet_ready` signal connected directly in GDScript to `AudioStreamNetwork.push_packet`. Hearing your own voice through the full pipeline = v0 done.
6. **Stats UI** in example: buffer size, expand events, packet loss, VAD state. Useful for debugging and as a template for game HUDs.
7. **README**: loopback setup, the "bring your own networking" story, pointers to godot-iroh and HLMP as next stepsthruth from .
8. **iroh example**: after the in-memory loopback is solid, add a focused example that sends voice over iroh datagrams in a direct peer-to-peer setup. This is the first concrete post-v0 networking milestone, ahead of any HLMP helper.

Past v0: HLMP router helper, iroh example, rnnoise, mute/deafen, tag/team routing, convenience "drop one node" wrapper. Order depends on what falls out of actually integrating HLMP and iroh with the v0 primitives.

## Open questions for future iterations

- **Convenience wrapper shape**: the v0 low-level API still requires manual scene setup and transport glue. A `SimpleMicVoiceSender` that handles the common microphone setup on `_ready` is an obvious v0.1 feature, but the right shape depends on how HLMP/iroh integration actually feels. Defer until we've used the low-level API in anger.
- **Input backend abstraction**: if `AudioServer.get_input_frames()` proves awkward on some platform, add a second backend using `AudioEffectCapture` rather than contorting the primary path.
- **Whether to vendor neteq** vs keep it as a git/crates dep. Decide after milestone 2: if the stress test passes and upstream looks healthy, stay on a pinned crates.io version; if the stress test reveals bugs we need to fix, vendor into `third_party/neteq/` and patch.
- **rnnoise**: `nnnoiseless` (pure Rust port) vs `rnnoise-sys` (C FFI). Both exist. Defer to when we actually want denoise.
- **Steam Audio fork scope**: once the basic voice path works, decide whether to patch `godot-steam-audio` for a cleaner mono-source path, lower audio-thread overhead, and runtime mix-rate handling, or keep voice-specific workarounds on our side.

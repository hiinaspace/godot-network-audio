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
- **Decision**: greenfield Rust, don't fork. Cross-reference twovoip when we hit mic sample-rate quirks, especially the 44.1/48 mismatch and `AudioServer.get_input_frames()` usage (added in Godot 4.6 specifically to fix mic reliability — twovoip's README calls this out). Also read `audio_stream_opus_chunked.cpp`'s `_mix_resampled` when implementing our playback's `_mix`.

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
8. **Sender is a `Node`**, not an `AudioEffect`, that holds a reference to a user-created `AudioEffectCapture`. Matches twovoip's working pattern, keeps us off the audio thread for encode, avoids finicky `AudioEffect` subclassing in gdext.

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
    /// Push interleaved f32 from AudioEffectCapture.
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
}
impl VoiceReceiver {
    pub fn new(sample_rate: u32) -> Result<Self>; // registers OpusAudioDecoder for PT 96
    pub fn push_packet(&mut self, pkt: VoicePacket);
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
    #[export] capture_effect: Option<Gd<AudioEffectCapture>>,
    #[export] bitrate: i32,             // 16000
    #[export] vad_threshold_db: f32,    // -45
    #[export] push_to_talk: bool,       // bypass VAD when true
    #[export] denoise: bool,
    encoder: Option<VoiceEncoder>,
}

#[godot_api]
impl INode for NetworkAudioSender {
    fn process(&mut self, _delta: f64) {
        // 1. Drain AudioEffectCapture::get_buffer() → encoder.push_pcm
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
    receiver: Arc<Mutex<VoiceReceiver>>,
}

#[godot_api]
impl AudioStreamNetwork {
    #[func] fn push_packet(&mut self, bytes: PackedByteArray) { /* ... */ }
    #[func] fn get_buffer_ms(&self) -> i32 { /* ... */ }
    #[func] fn get_stats(&self) -> Dictionary { /* ... */ }
}

// AudioStreamNetworkPlayback::_mix pulls 10 ms frames from receiver.
// Audio thread path; Mutex lock must be short (neteq tick is cheap).
```

### Framing detail

Godot's `AudioStreamPlayback::_mix` is called with a frame count from the audio driver — you must fill exactly that many, no preferred chunk size. NetEq produces fixed 10 ms frames (480 samples at 48 kHz). Standard pattern: the playback class keeps a small leftover buffer, each `_mix` drains leftover first, then pulls 10 ms frames until `dst` is full, stashes the tail. `two-voip-godot-4`'s `AudioStreamPlaybackOpusChunked::_mix_resampled` does exactly this — read it when implementing ours.

On input: `AudioEffectCapture::get_buffer(frames)` returns whatever the mic bus produces at whatever rate the audio server runs. Resample to 48 kHz inside `VoiceEncoder`, accumulate to 960-sample frames, encode.

## Milestones

1. **Skeleton**: workspace, `voice-core` compiles with `neteq = "=0.8.3"` (exact pin) and `audiopus`, plus `OpusAudioDecoder` impl. Unit test: encode 1 s sine → decode through receiver → assert RMS within tolerance of input.
2. **Sans-IO loopback stress test**: round-trip 10 s of recorded speech through encoder → receiver with simulated 5% loss + 20 ms jitter. Dump output wav and listen. **This is the go/no-go for the neteq bet** — if it sounds bad on realistic profiles, reassess before writing any gdext code.
3. **gdext scaffolding**: godot-rust crate builds a .so, registers `NetworkAudioSender` and `AudioStreamNetwork`, installs into `addons/`.
4. **Mic wire-up**: sender node actually pulls from `AudioEffectCapture` and emits `packet_ready`. Linux verified.
5. **Playback wire-up**: `AudioStreamNetwork._mix` pulls from `VoiceReceiver` into the dst buffer. Loopback demo scene: one `NetworkAudioSender`, `packet_ready` signal connected directly in GDScript to `AudioStreamNetwork.push_packet`. Hearing your own voice through the full pipeline = v0 done.
6. **Stats UI** in example: buffer size, expand events, packet loss, VAD state. Useful for debugging and as a template for game HUDs.
7. **README**: loopback setup, the "bring your own networking" story, pointers to godot-iroh and HLMP as next steps.

Past v0: HLMP router helper, iroh example, rnnoise, mute/deafen, tag/team routing, convenience "drop one node" wrapper. Order depends on what falls out of actually integrating HLMP and iroh with the v0 primitives.

## Open questions for future iterations

- **Convenience wrapper shape**: the v0 low-level API requires GDScript setup (bus + capture effect + sender node). A `SimpleMicVoiceSender` that creates all of that on `_ready` is an obvious v0.1 feature, but the right shape depends on how HLMP/iroh integration actually feels. Defer until we've used the low-level API in anger.
- **Whether to vendor neteq** vs keep it as a git/crates dep. Decide after milestone 2: if the stress test passes and upstream looks healthy, stay on a pinned crates.io version; if the stress test reveals bugs we need to fix, vendor into `third_party/neteq/` and patch.
- **rnnoise**: `nnnoiseless` (pure Rust port) vs `rnnoise-sys` (C FFI). Both exist. Defer to when we actually want denoise.

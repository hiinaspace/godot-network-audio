# godot-network-audio — design

This file records the durable architectural decisions behind the project.
For the current implementation queue, see `PLAN.md`.

## Scope

`godot-network-audio` handles the boundary between:
- hardware / engine audio frames
- encoded voice packets

It is intentionally not a full voice-chat product. The core library should stay:
- transport-agnostic
- game-policy-agnostic
- spatializer-agnostic

That means the project owns:
- microphone capture normalization
- paced Opus encoding
- packet parsing/formatting
- receive jitter buffering / PLC / reordering via NetEq
- Godot audio-stream playback integration

And it deliberately does not own:
- matchmaking / peer discovery
- room membership
- mute/deafen/team routing policy
- push-to-talk UX
- speaking indicators
- denoising policy
- one true networking backend

## Core architecture

There are three layers:

1. `voice-core`
- pure Rust, no Godot dependency
- owns packet format, encoder, decoder, NetEq-backed receiver, and sample-rate normalization helpers

2. `gdext`
- Godot-facing base addon
- owns:
  - `NetworkAudioSender`
  - `AudioStreamNetwork`
  - playback integration with Godot's audio server
- remains transport-agnostic

3. optional transport integrations
- separate crates/examples/addons for concrete networking backends
- first target: iroh/QUIC datagrams
- future possible target: HLMP example/integration

That split keeps the reusable audio logic independent from transport and keeps transport-specific runtime stacks out of the default addon.

## Audio model

### Send side

Canonical send format:
- mono
- 48 kHz
- 20 ms Opus frames

The sender pipeline is:

1. Godot-side capture pump
- reads microphone frames from `AudioServer.get_input_frames()`
- uses `AudioServer.get_input_mix_rate()`
- downmixes to mono and feeds a bounded PCM queue

2. paced send worker
- runs on a dedicated monotonic 20 ms cadence
- pulls exactly one canonical frame from the PCM queue
- encodes with Opus
- uses codec-native DTX for silence

3. egress fan-out
- default shape is one local encoded stream fanned out to all peers
- this matches the intended small iroh full-mesh use case

This is intentionally asymmetric with receive. There is one local sender pipeline per user, but one receive pipeline per remote talker.

### Receive side

The receive pipeline is:

1. transport ingress
- decode packet bytes into `VoicePacket`
- attach explicit packet arrival time in a monotonic domain
- route by authenticated remote peer identity
- enqueue into that peer's bounded queue

2. audio-thread playback
- owns `VoiceReceiver` / NetEq
- drains queued packets during mix
- pulls 10 ms NetEq frames
- fills Godot's requested output frame count exactly

The audio thread owns NetEq. Main and network threads must not lock or call into NetEq directly.

Each remote peer gets one stable `AudioStreamNetwork` and therefore one NetEq
instance. A game assigns that resource to its peer-specific 2D or 3D player.
The iroh network thread may enqueue directly, but stream creation/removal and
Godot node ownership stay on the main thread. Packets that beat peer
registration are held in a small bounded per-peer queue. Disconnect must stop
the corresponding player and remove the stream so Godot does not keep pulling
packet-loss concealment for a source that no longer exists.

### Silence and talkspurts

The core silence mechanism is Opus DTX, not manual packet suppression.

Reason:
- app-layer VAD packet dropping makes intentional silence look like packet loss
- NetEq expects a coherent packet stream, not arbitrary missing voiced frames

Practical implications:
- the sender should maintain regular encode cadence and let Opus decide when to enter DTX
- receiver logic must still handle talkspurt boundaries carefully
- the first packet after a DTX gap may need receiver flush/reset or a small resume prebuffer before normal playout resumes

### Sample rates

Input and output sample rates are separate concerns.

Input:
- query `AudioServer.get_input_mix_rate()` at runtime
- normalize to canonical 48 kHz mono before encode

Output:
- NetEq runs internally at 48 kHz
- playback should inherit `AudioStreamPlaybackResampled`
- Godot should own output-device resampling

The project should not rely on forcing the whole Godot project to 48 kHz just to function correctly.

## Godot integration model

### Why not `process()` for correctness

Game-frame scheduling is not a stable clock for voice transport.

`process()` can still be used for:
- draining Godot-owned mic frames into a raw PCM queue
- polling or surfacing stats
- demo glue

But it should not be the correctness boundary for:
- packet send pacing
- audio-thread receive timing
- transport ingress timing

If the game hitches, voice should degrade as little as possible.

### Why not a full custom Godot server yet

Godot's "server" model is real and appropriate for engine-global threaded systems, but it is heavier than needed for the first shipping addon shape.

The current intended progression is:
- base addon with explicit worker(s) and clear lifetimes
- optional shared service for sender-side fan-out
- only move to a more server-like singleton architecture if real usage shows the simpler ownership model is insufficient

For the near term, a shared local send service is enough. It should be conceptually server-like, but it does not need full RID-style engine integration yet.

## Transport architecture

### Base addon contract

The base addon should remain transport-agnostic.

Sender side:
- produce encoded packets
- expose them in a form transport code can consume without depending on per-frame GDScript polling for correctness

Receiver side:
- accept packet bytes plus explicit arrival timestamps
- keep packet transport details out of the audio layer
- keep one receive stream per audible remote source; mixing belongs to Godot

Current implementation note:
- the demo/harness currently also has a Rust-only loopback bypass for local testing
- that path is intentionally a harness convenience, not the intended long-term public transport boundary
- the optional iroh sidecar preserves remote peer identity and offers
  `get_or_create_receive_stream`, `remove_receive_stream`, and sender-side
  interest selection through `set_send_peers`

### Why iroh first

The first realistic target is small full-mesh p2p voice over iroh.

Reasons:
- it matches the user's actual intended application
- it avoids HLMP's polling-oriented runtime model as the first integration target
- it gives a realistic QUIC/datagram transport with RTT/stats visibility
- it is straightforward to run under Linux `tc netem`

### Recommended iroh shape

Do not bake iroh into the base addon.

Instead:
- keep `voice-core` and the base `gdext` addon transport-agnostic
- add an optional iroh-facing crate or addon layer

Recommended layers:

1. base addon
- `NetworkAudioSender`
- `AudioStreamNetwork`
- packet byte conversions

2. `godot-network-audio-iroh` sidecar
- owns iroh runtime and endpoint lifecycle
- owns the voice ALPN and protocol-version boundary
- owns peer connection map
- sends voice packets via QUIC datagrams
- receives datagrams and feeds `AudioStreamNetwork.push_packet_with_meta(...)`

Recommended session model:
- one iroh `Endpoint` per local Godot process
- one QUIC `Connection` per remote peer
- one voice ALPN for this protocol family and version, e.g. `godot-network-audio/voice/0`
- one bidirectional voice session per peer connection, carrying unreliable datagrams in both directions

Accept-side shape:
- register the voice ALPN on the iroh endpoint
- use a `Router` / `ProtocolHandler` or an equivalent dedicated accept loop owned by the sidecar
- when an incoming connection for the voice ALPN is accepted, spawn a datagram receive task for that peer and route packets into the correct `AudioStreamNetwork`
- if the same peer reconnects, the sidecar must explicitly replace or reject the old connection; iroh's router does not deduplicate accepted connections by peer identity for us

Why this shape:
- ALPN in iroh is the application-protocol boundary, not a room identifier
- peer identity and discovery are already tied to iroh endpoint IDs
- the sidecar should own connection reuse/deduplication, not force the Godot layer to reason about raw connections

### Full-mesh send model

For 2–8 peers, default to:
- one local mic capture path
- one paced sender pipeline
- one encoded packet stream
- fan-out of identical packet bytes to all connected peers

Do not start with one encoder per peer.

That is only needed later if per-peer adaptation becomes necessary:
- bitrate
- FEC policy
- redundancy

The correct upgrade path is:
- v1 shared encoded fan-out
- later optional per-peer encoder lanes fed from the same paced PCM frames

### iroh/QUIC specifics

Use unreliable datagrams, not streams, for voice payloads.

Useful iroh/QUIC signals to expose later:
- RTT
- connection stats
- max datagram size

What they are for:
- sender-side adaptation
- observability

What they are not for:
- replacing NetEq

QUIC does not replace:
- playout buffering
- jitter estimation
- packet-loss concealment
- talkspurt handling

NetEq remains the receive-side timing and concealment layer.

One explicit non-goal:
- do not copy `iroh-live`'s MoQ/WebTransport transport shape for this project

Why:
- `iroh-live` is solving a much broader live-media problem
- its transport is catalog/broadcast/track-oriented
- our voice path only needs small unreliable datagrams over a long-lived per-peer QUIC connection

Useful lessons from `iroh-live` that do carry over:
- deduplicate peer connections inside the transport layer
- keep transport/session ownership in a sidecar actor/service, not in UI glue
- treat 48 kHz as the internal media rate and resample at device boundaries

## Testing model

Testing should happen at three levels.

### 1. `voice-core` deterministic tests

In-process impairment tests around packet ingress:
- jitter
- reorder
- random loss
- burst loss
- silence/talkspurt transitions

These are the cheapest regression tests.

### 2. Godot audio-path harness

Single-process local loopback with PipeWire:
- virtual mic input
- captured output
- JSONL traces
- stats plots
- spectrogram comparison

This validates:
- Godot audio input/output integration
- sender pacing
- talkspurt handling
- audio-thread playback behavior

### 3. Real transport harness

Two-process transport tests:
- sender Godot instance
- receiver Godot instance
- real datagram transport between them
- `tc netem` impairment profiles

This is the correct place to validate the full iroh transport path.

## Current architectural conclusions

- The audio side is now good enough that transport integration is the right next frontier.
- The demo loopback path is useful as a regression harness even if it is not a product feature.
- The current direct loopback bypass is a testing path, not the model to expose as the real transport integration surface.
- The first real transport integration should be iroh, not HLMP.
- HLMP is still worth an example later, but should not drive the core design because of its polling-oriented runtime model.
- `PLAN.md` should stay tactical; this file should hold the durable reasoning.

# Embedding the voice extension

The default `standalone` feature exports the normal Godot entry point. A host
GDExtension can instead depend on this crate with `default-features = false`
and link it as an rlib; the host supplies the single Godot entry point.

`NetworkAudioSender::install_direct_send_handler` delivers encoded voice bytes
on the encoder worker. Keep the callback bounded and nonblocking; do not call
scene-tree methods from it. Installing a handler replaces `packet_ready`.

Create one `AudioStreamNetwork` per remote speaker on the Godot thread and
retain `stream.bind().loopback_target()` for the transport. Its
`enqueue_bytes_at(bytes, Instant)` accepts original monotonic arrival time,
decodes the bounded voice packet, and enqueues it for the audio-thread NetEq
owner. The transport must never drive NetEq itself. Play the stream through the
application's audio player/spatializer and remove it when that peer leaves.

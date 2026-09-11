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

For capture independent of scene rendering, set `capture_on_worker = true`
before `start_capture()`. This gives the encoder worker exclusive ownership of
AudioServer's input reads; do not create another capture reader concurrently.
Stop capture before selecting a different input device. `stop_capture()` joins
the worker before deactivating the driver and discards pending encoded audio.

`set_input_gain_db()` applies -30 to +24 dB before encoding and limits samples to
[-1, 1]. `get_input_peak_db()` reports the post-gain input meter; `is_capturing()`
reports activation success. Microphone capture is opt-in and defaults off.

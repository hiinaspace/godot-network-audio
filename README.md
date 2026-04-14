# godot-network-audio (WIP)

A Godot game engine extension for sending and receiving audio over data packets, aka VOIP. 

Specifically, this extension does the stuff you need to do between Godot's
audio system and data packets to transmit audio efficiently and robustly, but leaves
the packet delivery part up to your choice of transport (up to some practical requirements).

Status: works quite well in varying network conditions, at least inside some
test harnesses. API isn't quite usable in an actual game setting though. no
builds yet.

## Rough overview

There's a bunch of stuff you have to do to be able to speak into a microphone
on one end of the internet and have speakers play out sort of the same thing.
The parts between the hardware and your game code are already pretty well solved
by Godot's audio system, but if you want to take the raw PCM data you get from
that and put it through the internet, you're generally on your own.

Part of this is compressing/encoding the raw PCM into something you can reasonably
fit on the internet. Opus is a good codec for this. However, even if you encode
your audio into opus packets, you still need a way to handle those packets
such that the speaker on the receiving end can produce similar audio to the input,
despite all the things that can happen to those packets while in transit.

The WebRTC implementation of networked audio which is widely deployed in web
browsers has a subsystem called "netEQ" that handles the part of the stack
between opus packets and the raw PCM output to your sound card. It's fairly
sophisticated and can smooth over a lot of the artifacts you get if you just
decoded the opus packets whenever you got them and pushed the PCM to the output buffer.
This extension links to a rust reimplementation of neteq's algorithm, which does
the heavy lifting.

The rest of the extension is plumbing between neteq, opus codec, and godot's
audio system, and an actual transport layer, but still in a way generally
robust to whatever the main process() thread of godot is up to. So e.g. if you
tab out of the godot game and it throttles process callbacks to 5fps, voice
will still transmit.

### A bunch of test harnesses

One thing you'll quickly grow tired of when hacking on voip stuff is trying to
speak into your mic and hear the loopback as manual validation.

So instead this repo has as bunch of scripts that will setup virtual audio
inputs/outputs (in pipewire on linux), play a test clip on repeat as input, and
capture the audio from the other end, past all of godot, this extension, and
the (also simulated loopback) network, while also collecting metrics. And then it can
plot the metrics + some spectrogram pictures of input/output, so you can also
visually scan for artifacts instead of listening to your test clip over and over.

## But what if I want working voice chat in a godot game right now?

If you don't mind hosting some separate software (apart from your game server)
https://github.com/NodotProject/godot-livekit seems promising (livekit itself
is pretty widely deployed).

There's also https://github.com/goatchurchprime/two-voip-godot-4 , which roughly
fills the same part as this repo's attempt (godot audio server into opus with some
sending/jitter/playout buffers, then you do what you want with the bytes).


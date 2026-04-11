make voice chat work reasonably in godot.

https://github.com/goatchurchprime/two-voip-godot-4 is an attempt at it, but it feels like it's missing some pieces.

The shape I'm imagining from user/gamedev perspective is an AudioStreamNetwork node, which fits into a stream player for playback. The node plays silence by default. But you can feed it NetworkAudioPackets, which are essentially RTP packets with opus audio in them (but not necessarily conforming), then the AudioStreamNetwork node handles decoding, packet loss concealment, jitter and playout buffering internally, sort of like the NetEQ library used inside webrtc. 

The NetworkAudioPackets in turn can be serialized over godot's high level networking RPCs in 'unrealiable' mode. Some sort of NetworkAudioReceiver node has the RPCs defined on it internally and also handles state like congestion detection/adaptation. Or maybe since the adaptation sort of spans both the networking and the jitter/playout buffers, maybe the NetworkAudioReceiver is also the AudioStream node itself.

And on the sending side, a NetworkAudioSender also references an AudioEffectCapture node (which presumably but not necessarily is on a bus with the AudioStreamMicrophone), and produces the NetworkAudioPackets, does voice activity detection (or maybe defers the fancier denoising to an AudioEffect, but at least can signal empty audio properly to the receiver) and does any adaptation for congestion from the RTP-like data in the packets.

Or thinking about how godot's high-level networking works, the NetworkAudioSender/Receiver should actually be a single AudioStreamNetwork node, which gets spawned for each multiplayer peer on all peers with one that has authority. the authority then sets it to sending mode with the source AudioEffectCapture. so then the internal rpcs that send the audio packets are received on the remote peers. 

Hmm, I think the rpcs work by getting broadcast by the server aka peer 1. so maybe congestion detection isn't really peer to peer but sending peer to server and server to receiving peer. The rpcs can be addressed to a specific peer id. So maybe the AudioStreamNetwork node has a dev-facing way to add/remove interested receiver peers (as separate rpcs that are broadcast reliably), then when voice is detected (+ any push-to-talk like boolean), send an rpc to peers individually, and that receiving peer also sends back rtp-style confirmations as a separate rpc. so can still do congestion control, even if there's a server in the middle. 

Or alternatively could have the AudioStreamNetwork node send the packets to the server, and have the server node handle the routing and congestion control for individual peers.

Pretty weird networking model. But I think this way the important details of voip like activity detection/encoding/congestion adaptation/routing/jitter and playback buffering/decoding all happen internally, the authority/ownership model still matches godot's high-level peers, the low-level networking doesn't matter (sending "unreliable" rtp-like packets over whatever) and the audio still flows in through AudioCapture and out through a AudioPlayback, so it's compatible with microphones and 3d spatialization.

https://github.com/adrenak/univoice might be a good reference for what a high-level voice api looks like.

I'm unsure how complex the congestion adaptation and jitter buffer parts need to be, and how much can be delegated to a library. webrtc is open source and license compatible, and maybe the webEQ part is usable on its own. there's also a rust impl at https://github.com/security-union/videocall-rs/tree/main/neteq though it's bleeding edge. We certainly don't need to be as robust to varying networks as webrtc itself, though OTOH a networked game still runs on networks, so it can't be ignored. 

Another place to consider alternatives is trying to use godot's high-level networking rpcs and authority. Maybe the mid or low level networking peer api would be better; still using the channels/peer ids (so if you can talk to a peer for other game networking, you can then also send/receive audio). 

Could also not integrate with the godot networking at all, only use it for signaling an actual webrtc connection. I think https://github.com/paullouisageneau/libdatachannel despite its name also implements audio channels now. Though this shape is already pretty much covered by https://github.com/NodotProject/godot-livekit , which is an SFU instead of full mesh p2p, but does just work.

I do think full mesh p2p is nice though. specifically with https://github.com/tipragot/godot-iroh for hole-punching and addressing, can then avoid a lot of webrtc shaped complexity if you don't need strict compatability with other webrtc infra. What's unclear there is how amenable godot's high-level networking is to true p2p instead of authoritative server. It seems like you can basically do full mesh p2p with a client/server (aka peer id 1) per pair. but then could also just do iroh endpoints/peers directly (which is essentially QUIC + holepunching).

Hmm, the `AudioStreamNetwork` part of wrapping AudioCapture, opus codec, voice activity, packet loss concealment, and jitter/playback buffer, and AudioStream into some sort of sans-IO state machine is valuable on its own. And then for devs happy with the godot high-level client-sever model, some sort of  `HighLevelNetworkingAudioHandler` thing can handle the rpc and authority and routing parts; which I could build or just leave as future work. And then for my preferred p2p case, can use the low-level iroh endpoints from godot-iroh to send and receive the rtp-like packets (and do other game networking some other way).

So it's less "make voice chat reasonably in godot" but more "make voice to/from packets possible in godot", leaving the networking undefined. since client/sever high-level networking and iroh p2p full mesh (and livekit SFU) are both valid answers to "reasonable voice chat" with different underlying assumptions. Could still build out one/both in the same extension/repo but don't have to initially.

then I think main question is how much can be vendored from existing libraries vs built in the extension. certainly will use opus. probably can use rnnoise (https://github.com/werman/noise-suppression-for-voice/ is gpl, but rnnoise itself is bsd). maybe use webrtc's neteq, or reimplement the relevant parts. I think if the library looks like it'll be thin, just gluing together parts, then can use https://github.com/godotengine/godot-cpp-template/ . if it looks like it'll be fatter, then https://github.com/godot-rust/gdext instead, rust will be more interesting from an impl standpoint, and no more cmake wrangling.

It is possible somebody would want to use this for more general audio transmission (e.g. music like sonobus), but definitely MVP is voice and probably worth having VAD in by default.

https://github.com/godotengine/godot-proposals/issues/870 related discussion; I think this might be a good factoring of it.

https://github.com/godotengine/webrtc-native huh, this does link to libdatachannel.
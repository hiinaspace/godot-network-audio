use godot::prelude::*;

mod packet_bytes;
mod sender;
mod stream;

struct GodotNetworkAudioExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GodotNetworkAudioExtension {}

use godot::prelude::*;

mod packet_bytes;
mod sender;
mod stream;

#[cfg(feature = "iroh-transport")]
mod transport_iroh;

struct GodotNetworkAudioExtension;

#[gdextension]
unsafe impl ExtensionLibrary for GodotNetworkAudioExtension {}

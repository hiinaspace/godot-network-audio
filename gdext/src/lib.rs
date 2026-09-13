#[cfg(feature = "standalone")]
use godot::prelude::*;

mod packet_bytes;
mod pcm_tap;
pub use pcm_tap::{PcmBlock, PcmTap};
mod sender;
mod stream;

#[cfg(feature = "iroh-transport")]
mod transport_iroh;

pub use sender::{DirectSendHandler, NetworkAudioSender};
pub use stream::{AudioStreamNetwork, LoopbackTarget as NetworkAudioIngress};

#[cfg(feature = "standalone")]
struct GodotNetworkAudioExtension;

#[cfg(feature = "standalone")]
#[gdextension]
unsafe impl ExtensionLibrary for GodotNetworkAudioExtension {}

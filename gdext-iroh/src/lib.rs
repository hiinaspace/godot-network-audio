mod transport;

use godot::prelude::*;

struct IrohVoiceExtension;

#[gdextension]
unsafe impl ExtensionLibrary for IrohVoiceExtension {}

//! The previous NetEq-based receive path, kept only for the old Godot client (gdext).
//!
//! Behind the `legacy-neteq` feature so the prim binaries stay NetEq-free. Frozen at the
//! behaviour of 56b4c31; do not extend it. New code uses [`crate::playout::PlayoutReceiver`].

pub mod decoder;
pub mod harness;
pub mod receiver;

pub use decoder::OpusAudioDecoder;
pub use receiver::{ReceiverStats, VoiceReceiver};

/// Arrival stamp for the legacy receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketArrival {
    pub received_at_mono_us: u64,
}

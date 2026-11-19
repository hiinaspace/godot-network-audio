//! Prim's voice codec and receive path: 20 ms Opus frames on a 48 kHz timeline, a
//! timeline-driven jitter buffer with Opus concealment, and the microphone-side encoder.

pub mod decoder;
pub mod delay;
pub mod encoder;
pub mod error;
pub mod packet;
pub mod playout;
pub mod resample;
pub mod time_stretch;

/// Offline simulated-clock impairment runs (tests and examples only).
#[cfg(feature = "test-support")]
pub mod fixture_harness;

/// The previous NetEq receiver, for the old Godot client only (feature `legacy-neteq`).
#[cfg(feature = "legacy-neteq")]
pub mod legacy;

pub use decoder::OpusAudioDecoder;
pub use encoder::{VoiceEncoder, VoiceEncoderConfig};
pub use error::{Error, Result};
pub use packet::{PacketFlags, VoicePacket};
pub use playout::{
    PacketObservation, PlayoutConfig, PlayoutReceiver, PlayoutStats, PushOutcome,
};

#[cfg(feature = "legacy-neteq")]
pub use legacy::{PacketArrival, ReceiverStats, VoiceReceiver};

/// The linked libopus version string, e.g. `"libopus 1.5.2"`.
pub fn opus_version() -> &'static str {
    audiopus::version()
}

#[cfg(test)]
mod tests {
    /// Every build (desktop and aarch64) links the one vendored libopus.
    #[test]
    fn links_the_vendored_libopus_1_5_2() {
        assert_eq!(super::opus_version(), "libopus 1.5.2");
    }
}

//! Opus decoding for one 20 ms / 48 kHz / mono slot at a time, with the three ways a slot can
//! be produced: from its own packet, from the next packet's in-band FEC, or by concealment.

use audiopus::coder::{Decoder as OpusDecoder, GenericCtl};
use audiopus::packet::{nb_channels, nb_samples, Packet};
use audiopus::MutSignals;
use audiopus::{Channels, SampleRate};

use crate::error::{Error, Result};

/// Samples in one slot: 20 ms at 48 kHz.
pub const SLOT_SAMPLES: usize = 960;

/// Check that `payload` is exactly one 20 ms, 48 kHz, mono Opus frame (CONTRACTS section 6).
pub fn validate_payload(payload: &[u8]) -> Result<()> {
    let packet =
        Packet::try_from(payload).map_err(|_| Error::InvalidPacket("empty opus payload"))?;
    let samples = nb_samples(packet, SampleRate::Hz48000)
        .map_err(|_| Error::InvalidPacket("undecodable opus packet"))?;
    if samples != SLOT_SAMPLES {
        return Err(Error::InvalidPacket("opus packet is not 20 ms"));
    }
    let packet = Packet::try_from(payload).expect("checked above");
    match nb_channels(packet) {
        Ok(Channels::Mono) => Ok(()),
        _ => Err(Error::InvalidPacket("opus packet is not mono")),
    }
}

/// Whether the packet carries LBRR (in-band FEC) data for the frame before it.
pub fn payload_has_fec(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }
    // SAFETY: pointer and length describe the live slice.
    unsafe { audiopus::ffi::opus_packet_has_lbrr(payload.as_ptr(), payload.len() as i32) == 1 }
}

pub struct OpusAudioDecoder {
    inner: OpusDecoder,
}

impl OpusAudioDecoder {
    /// A 48 kHz mono decoder.
    pub fn new() -> Result<Self> {
        let inner = OpusDecoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| Error::Opus(format!("decoder init: {e}")))?;
        Ok(Self { inner })
    }

    /// Decode the frame in `payload` into `out` (exactly [`SLOT_SAMPLES`] long).
    pub fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> Result<usize> {
        self.run(Some(payload), out, false)
    }

    /// Recover the frame *before* `next_payload` from its LBRR data. Call this, then
    /// [`decode`](Self::decode) the same payload for its own frame.
    pub fn decode_fec(&mut self, next_payload: &[u8], out: &mut [f32]) -> Result<usize> {
        self.run(Some(next_payload), out, true)
    }

    /// Opus packet-loss concealment for one lost frame. Opus fades its own output when
    /// called repeatedly.
    pub fn conceal(&mut self, out: &mut [f32]) -> Result<usize> {
        self.run(None, out, false)
    }

    pub fn reset(&mut self) -> Result<()> {
        self.inner
            .reset_state()
            .map_err(|e| Error::Opus(format!("reset: {e}")))
    }

    fn run(&mut self, payload: Option<&[u8]>, out: &mut [f32], fec: bool) -> Result<usize> {
        debug_assert_eq!(out.len(), SLOT_SAMPLES);
        let input = match payload {
            Some(p) => Some(Packet::try_from(p).map_err(|_| Error::InvalidPacket("empty"))?),
            None => None,
        };
        let output = MutSignals::try_from(out)
            .map_err(|e| Error::Opus(format!("output buffer: {e}")))?;
        self.inner
            .decode_float(input, output, fec)
            .map_err(|e| Error::Opus(format!("decode: {e}")))
    }
}

use audiopus::coder::Encoder as OpusEncoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};

use crate::error::{Error, Result};
use crate::packet::{PacketFlags, VoicePacket};
use crate::resample::InputResampler;
use crate::vad::{EnergyVad, VadConfig};

const FRAME_DURATION_MS: u32 = 20;
const FRAME_SAMPLES_48K_MONO: usize = 960;
const MAX_PACKET_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct VoiceEncoderConfig {
    pub input_sample_rate: u32,
    pub frame_duration_ms: u32,
    pub bitrate_bps: i32,
    pub vad: VadConfig,
    pub denoise: bool,
}

impl Default for VoiceEncoderConfig {
    fn default() -> Self {
        Self {
            input_sample_rate: 48_000,
            frame_duration_ms: FRAME_DURATION_MS,
            bitrate_bps: 16_000,
            vad: VadConfig::default(),
            denoise: false,
        }
    }
}

pub struct VoiceEncoder {
    encoder: OpusEncoder,
    resampler: InputResampler,
    vad: EnergyVad,
    pending_pcm: Vec<f32>,
    force_transmit: bool,
    seq: u16,
    timestamp: u32,
    speaking: bool,
}

impl VoiceEncoder {
    pub fn new(config: VoiceEncoderConfig) -> Result<Self> {
        if config.frame_duration_ms != FRAME_DURATION_MS {
            return Err(Error::UnsupportedConfig(
                "milestone 1 only supports 20 ms opus frames",
            ));
        }
        if config.denoise {
            return Err(Error::UnsupportedConfig(
                "denoise is not implemented in milestone 1",
            ));
        }

        let mut encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| Error::Opus(format!("encoder init: {e}")))?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(config.bitrate_bps))
            .map_err(|e| Error::Opus(format!("set bitrate: {e}")))?;
        encoder
            .set_vbr(true)
            .map_err(|e| Error::Opus(format!("enable vbr: {e}")))?;

        Ok(Self {
            encoder,
            resampler: InputResampler::new(config.input_sample_rate, 48_000)?,
            vad: EnergyVad::new(config.vad),
            pending_pcm: Vec::new(),
            force_transmit: false,
            seq: 0,
            timestamp: 0,
            speaking: false,
        })
    }

    pub fn push_pcm(&mut self, samples: &[f32]) {
        self.resampler.process(samples, &mut self.pending_pcm);
    }

    pub fn poll_packet(&mut self) -> Result<Option<VoicePacket>> {
        if self.pending_pcm.len() < FRAME_SAMPLES_48K_MONO {
            return Ok(None);
        }

        // Milestone 1 keeps this simple. Replace this per-frame allocation with a reused
        // scratch buffer before wiring the encoder into real-time engine paths.
        let frame: Vec<f32> = self.pending_pcm.drain(..FRAME_SAMPLES_48K_MONO).collect();
        let voiced = self.force_transmit || self.vad.is_voiced(&frame);

        if !voiced {
            if self.speaking {
                self.speaking = false;
                return Ok(Some(VoicePacket {
                    seq: self.next_seq(),
                    timestamp: self.next_timestamp(),
                    flags: PacketFlags::from_bits(PacketFlags::END_OF_TALKSPURT),
                    payload: Vec::new(),
                }));
            }
            return Ok(None);
        }

        let mut payload = vec![0_u8; MAX_PACKET_BYTES];
        let packet_len = self
            .encoder
            .encode_float(&frame, &mut payload)
            .map_err(|e| Error::Opus(format!("encode: {e}")))?;
        payload.truncate(packet_len);

        let mut flags = PacketFlags::default();
        if !self.speaking {
            flags.set(PacketFlags::START_OF_TALKSPURT, true);
            self.speaking = true;
        }

        Ok(Some(VoicePacket {
            seq: self.next_seq(),
            timestamp: self.next_timestamp(),
            flags,
            payload,
        }))
    }

    pub fn set_force_transmit(&mut self, on: bool) {
        self.force_transmit = on;
    }

    pub fn flush(&mut self) {
        self.pending_pcm.clear();
        self.speaking = false;
    }

    fn next_seq(&mut self) -> u16 {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        seq
    }

    fn next_timestamp(&mut self) -> u32 {
        let ts = self.timestamp;
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES_48K_MONO as u32);
        ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_opus_packet_for_voiced_frame() {
        let mut enc = VoiceEncoder::new(VoiceEncoderConfig::default()).unwrap();
        enc.push_pcm(&vec![0.1; FRAME_SAMPLES_48K_MONO]);

        let pkt = enc.poll_packet().unwrap().unwrap();
        assert!(!pkt.payload.is_empty());
        assert!(pkt.flags.contains(PacketFlags::START_OF_TALKSPURT));
    }
}

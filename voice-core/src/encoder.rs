use audiopus::coder::{Encoder as OpusEncoder, GenericCtl};
use audiopus::{Application, Bitrate, Channels, SampleRate};

use crate::error::{Error, Result};
use crate::packet::{PacketFlags, VoicePacket};
use crate::resample::InputResampler;

const FRAME_DURATION_MS: u32 = 20;
const FRAME_SAMPLES_48K_MONO: usize = 960;
const MAX_PACKET_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct VoiceEncoderConfig {
    pub input_sample_rate: u32,
    pub frame_duration_ms: u32,
    pub bitrate_bps: i32,
    pub enable_dtx: bool,
    pub denoise: bool,
}

impl Default for VoiceEncoderConfig {
    fn default() -> Self {
        Self {
            input_sample_rate: 48_000,
            frame_duration_ms: FRAME_DURATION_MS,
            bitrate_bps: 16_000,
            enable_dtx: true,
            denoise: false,
        }
    }
}

pub struct VoiceEncoder {
    encoder: OpusEncoder,
    resampler: InputResampler,
    pending_pcm: Vec<f32>,
    seq: u16,
    timestamp: u32,
    speaking: bool,
    enable_dtx: bool,
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
        encoder
            .set_dtx(config.enable_dtx)
            .map_err(|e| Error::Opus(format!("set dtx: {e}")))?;

        Ok(Self {
            encoder,
            resampler: InputResampler::new(config.input_sample_rate, 48_000)?,
            pending_pcm: Vec::new(),
            seq: 0,
            timestamp: 0,
            speaking: false,
            enable_dtx: config.enable_dtx,
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
        let mut payload = vec![0_u8; MAX_PACKET_BYTES];
        let packet_len = self
            .encoder
            .encode_float(&frame, &mut payload)
            .map_err(|e| Error::Opus(format!("encode: {e}")))?;
        let timestamp = self.timestamp;
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES_48K_MONO as u32);

        // With DTX enabled, Opus may return tiny no-send packets during silence.
        if self.enable_dtx && packet_len <= 2 {
            if self.speaking {
                self.speaking = false;
                return Ok(Some(VoicePacket {
                    seq: self.next_seq(),
                    timestamp,
                    flags: PacketFlags::from_bits(PacketFlags::END_OF_TALKSPURT),
                    payload: Vec::new(),
                }));
            }
            return Ok(None);
        }
        payload.truncate(packet_len);

        let mut flags = PacketFlags::default();
        if !self.speaking {
            flags.set(PacketFlags::START_OF_TALKSPURT, true);
            self.speaking = true;
        }

        Ok(Some(VoicePacket {
            seq: self.next_seq(),
            timestamp,
            flags,
            payload,
        }))
    }

    pub fn flush(&mut self) {
        self.pending_pcm.clear();
        self.speaking = false;
        let _ = self.encoder.reset_state();
    }

    /// Advance the media clock for capture frames dropped before encoding.
    ///
    /// RTP timestamps track captured sample time, not transmitted packet count.
    /// Sequence numbers therefore remain unchanged.
    pub fn advance_dropped_frames(&mut self, frames: u64) {
        self.timestamp = self
            .timestamp
            .wrapping_add((frames as u32).wrapping_mul(FRAME_SAMPLES_48K_MONO as u32));
    }

    fn next_seq(&mut self) -> u16 {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        seq
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

    #[test]
    fn dtx_silence_advances_timestamp_without_consuming_sequences() {
        let mut enc = VoiceEncoder::new(VoiceEncoderConfig::default()).unwrap();
        enc.push_pcm(&[0.1; FRAME_SAMPLES_48K_MONO]);
        let first = enc.poll_packet().unwrap().unwrap();

        let silent_frames = 100_u32;
        let mut last_silence_sequence = first.seq;
        for _ in 0..silent_frames {
            enc.push_pcm(&[0.0; FRAME_SAMPLES_48K_MONO]);
            if let Some(packet) = enc.poll_packet().unwrap() {
                last_silence_sequence = packet.seq;
            }
        }

        enc.push_pcm(&[0.1; FRAME_SAMPLES_48K_MONO]);
        let resumed = enc.poll_packet().unwrap().unwrap();
        assert_eq!(
            resumed.timestamp.wrapping_sub(first.timestamp),
            (silent_frames + 1) * FRAME_SAMPLES_48K_MONO as u32
        );
        assert_eq!(resumed.seq, last_silence_sequence.wrapping_add(1));
    }

    #[test]
    fn dropped_capture_frames_advance_only_timestamp() {
        let mut enc = VoiceEncoder::new(VoiceEncoderConfig {
            enable_dtx: false,
            ..VoiceEncoderConfig::default()
        })
        .unwrap();
        enc.push_pcm(&[0.1; FRAME_SAMPLES_48K_MONO]);
        let first = enc.poll_packet().unwrap().unwrap();
        enc.advance_dropped_frames(3);
        enc.push_pcm(&[0.1; FRAME_SAMPLES_48K_MONO]);
        let second = enc.poll_packet().unwrap().unwrap();

        assert_eq!(second.seq, first.seq.wrapping_add(1));
        assert_eq!(
            second.timestamp.wrapping_sub(first.timestamp),
            4 * FRAME_SAMPLES_48K_MONO as u32
        );
    }
}

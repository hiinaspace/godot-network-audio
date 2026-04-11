use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
use web_time::{Duration, Instant};

use crate::decoder::OpusAudioDecoder;
use crate::error::{Error, Result};
use crate::packet::{PacketArrival, VoicePacket};

const PAYLOAD_TYPE_OPUS: u8 = 96;
const SSRC_FIXED: u32 = 0x474e_6175;

#[derive(Debug, Clone, Default)]
pub struct ReceiverStats {
    pub current_buffer_size_ms: u32,
    pub target_delay_ms: u32,
    pub packets_awaiting_decode: usize,
    pub expand_rate: u16,
    pub accelerate_rate: u16,
    pub concealed_samples: u64,
    pub consecutive_failures: u32,
    pub sticky_error: Option<String>,
}

pub struct VoiceReceiver {
    inner: NetEq,
    sample_rate: u32,
    channels: u8,
    arrival_epoch_mono_us: Option<u64>,
    arrival_epoch_instant: Option<Instant>,
    consecutive_failures: u32,
    sticky_error: Option<String>,
}

impl VoiceReceiver {
    pub fn new(sample_rate: u32) -> Result<Self> {
        let channels = 1;
        let mut inner = NetEq::new(NetEqConfig {
            sample_rate,
            channels,
            min_delay_ms: 20,
            max_delay_ms: 250,
            ..Default::default()
        })?;
        inner.register_decoder(
            PAYLOAD_TYPE_OPUS,
            Box::new(OpusAudioDecoder::new(sample_rate, channels)?),
        );

        Ok(Self {
            inner,
            sample_rate,
            channels,
            arrival_epoch_mono_us: None,
            arrival_epoch_instant: None,
            consecutive_failures: 0,
            sticky_error: None,
        })
    }

    pub fn push_packet(&mut self, pkt: VoicePacket, arrival: PacketArrival) -> Result<()> {
        if pkt.payload.is_empty() {
            return Ok(());
        }

        let header = RtpHeader::new(pkt.seq, pkt.timestamp, SSRC_FIXED, PAYLOAD_TYPE_OPUS, false);
        let mut packet = AudioPacket::new(header, pkt.payload, self.sample_rate, self.channels, 20);
        packet.arrival_time = self.instant_for_arrival(arrival.received_at_mono_us);

        self.inner.insert_packet(packet).map_err(|err| {
            self.record_failure(format!("insert_packet: {err}"));
            Error::from(err)
        })?;

        Ok(())
    }

    pub fn pull_frame(&mut self, out: &mut [f32]) {
        match self.inner.get_audio() {
            Ok(frame) => {
                self.consecutive_failures = 0;
                let n = out.len().min(frame.samples.len());
                out[..n].copy_from_slice(&frame.samples[..n]);
                out[n..].fill(0.0);
            }
            Err(err) => {
                out.fill(0.0);
                self.record_failure(format!("get_audio: {err}"));
            }
        }
    }

    pub fn stats(&self) -> ReceiverStats {
        let stats = self.inner.get_statistics();
        ReceiverStats {
            current_buffer_size_ms: stats.current_buffer_size_ms,
            target_delay_ms: stats.target_delay_ms,
            packets_awaiting_decode: stats.packets_awaiting_decode,
            expand_rate: stats.network.expand_rate,
            accelerate_rate: stats.network.accelerate_rate,
            concealed_samples: stats.lifetime.concealed_samples,
            consecutive_failures: self.consecutive_failures,
            sticky_error: self.sticky_error.clone(),
        }
    }

    fn instant_for_arrival(&mut self, received_at_mono_us: u64) -> Instant {
        let epoch_us = *self
            .arrival_epoch_mono_us
            .get_or_insert(received_at_mono_us);
        let epoch_instant = *self.arrival_epoch_instant.get_or_insert_with(Instant::now);
        let delta_us = received_at_mono_us.saturating_sub(epoch_us);
        epoch_instant + Duration::from_micros(delta_us)
    }

    fn record_failure(&mut self, message: String) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.sticky_error.is_none() {
            self.sticky_error = Some(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result as AnyResult;

    use super::*;
    use crate::encoder::{VoiceEncoder, VoiceEncoderConfig};

    const FRAME_SAMPLES: usize = 960;
    const SAMPLE_RATE: u32 = 48_000;

    #[test]
    fn encode_decode_sine_through_receiver_keeps_rms_reasonable() -> AnyResult<()> {
        let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;

        let input = sine_wave_seconds(1.0, 440.0, SAMPLE_RATE, 0.2);
        let input_rms = rms(&input);

        let mut output = Vec::new();
        let mut arrival_us = 0_u64;

        for chunk in input.chunks(FRAME_SAMPLES) {
            encoder.push_pcm(chunk);
            if let Some(pkt) = encoder.poll_packet()? {
                receiver.push_packet(
                    pkt,
                    PacketArrival {
                        received_at_mono_us: arrival_us,
                    },
                )?;
            }

            for _ in 0..2 {
                let mut frame = vec![0.0; FRAME_SAMPLES];
                receiver.pull_frame(&mut frame);
                output.extend_from_slice(&frame);
            }

            arrival_us += 20_000;
        }

        let output_rms = rms(&output);
        let ratio = output_rms / input_rms;
        assert!(
            ratio > 0.60 && ratio < 1.40,
            "unexpected rms ratio: {ratio}"
        );

        let stats = receiver.stats();
        assert_eq!(stats.consecutive_failures, 0);
        assert!(stats.sticky_error.is_none());
        Ok(())
    }

    fn sine_wave_seconds(seconds: f32, hz: f32, sample_rate: u32, amplitude: f32) -> Vec<f32> {
        let total_samples = (seconds * sample_rate as f32) as usize;
        (0..total_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * hz * t).sin() * amplitude
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }
}

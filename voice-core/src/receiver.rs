use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
use web_time::{Duration, Instant};

use crate::decoder::OpusAudioDecoder;
use crate::error::{Error, Result};
use crate::packet::{PacketArrival, VoicePacket};

const PAYLOAD_TYPE_OPUS: u8 = 96;
const SSRC_FIXED: u32 = 0x474e_6175;
const OUTPUT_FRAME_MS: u32 = 10;
const TALKSPURT_DRAIN_ROUNDING_TOLERANCE_MS: u32 = 2;
const TALKSPURT_RESUME_PREBUFFER_PACKETS: u32 = 2;
const OPUS_PACKET_SAMPLES: u32 = 960;
const DTX_DISCONTINUITY_MIN_MISSING_FRAMES: u32 = 5;

#[derive(Debug, Clone, Default)]
pub struct ReceiverStats {
    pub current_buffer_size_ms: u32,
    pub target_delay_ms: u32,
    pub preferred_buffer_size_ms: u32,
    pub packets_awaiting_decode: usize,
    pub packets_per_sec: u32,
    pub expand_rate: u16,
    pub accelerate_rate: u16,
    pub preemptive_rate: u16,
    pub expand_per_sec: f32,
    pub accelerate_per_sec: f32,
    pub preemptive_expand_per_sec: f32,
    pub normal_per_sec: f32,
    pub concealed_samples: u64,
    pub concealment_events: u64,
    pub silent_concealed_samples: u64,
    pub late_packets_discarded: u64,
    pub inserted_samples_for_deceleration: u64,
    pub removed_samples_for_acceleration: u64,
    pub consecutive_failures: u32,
    pub sticky_error: Option<String>,
    pub intentional_silence: bool,
    pub playout_paused: bool,
}

pub struct VoiceReceiver {
    inner: NetEq,
    sample_rate: u32,
    channels: u8,
    consecutive_failures: u32,
    sticky_error: Option<String>,
    pending_silence: bool,
    intentional_silence: bool,
    end_of_talkspurt_seq: Option<u16>,
    resuming_talkspurt: bool,
    resumed_packet_count: u32,
    last_media_seq: Option<u16>,
    last_media_timestamp: Option<u32>,
}

impl VoiceReceiver {
    pub fn new(sample_rate: u32) -> Result<Self> {
        Self::new_with_delay_bounds(sample_rate, 20, 250)
    }

    pub fn new_with_delay_bounds(
        sample_rate: u32,
        min_delay_ms: u32,
        max_delay_ms: u32,
    ) -> Result<Self> {
        let channels = 1;
        let mut inner = NetEq::new(NetEqConfig {
            sample_rate,
            channels,
            min_delay_ms,
            max_delay_ms,
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
            consecutive_failures: 0,
            sticky_error: None,
            pending_silence: false,
            intentional_silence: false,
            end_of_talkspurt_seq: None,
            resuming_talkspurt: false,
            resumed_packet_count: 0,
            last_media_seq: None,
            last_media_timestamp: None,
        })
    }

    pub fn push_packet(&mut self, pkt: VoicePacket, arrival: PacketArrival) -> Result<()> {
        self.push_packet_with_simulated_now(pkt, arrival, arrival.received_at_mono_us)
    }

    pub fn push_packet_with_now_mono(
        &mut self,
        pkt: VoicePacket,
        arrival: PacketArrival,
        now_mono_us: u64,
    ) -> Result<()> {
        self.push_packet_with_simulated_now(pkt, arrival, now_mono_us)
    }

    fn push_packet_with_simulated_now(
        &mut self,
        pkt: VoicePacket,
        arrival: PacketArrival,
        simulated_now_mono_us: u64,
    ) -> Result<()> {
        let explicit_start = pkt
            .flags
            .contains(crate::packet::PacketFlags::START_OF_TALKSPURT);
        let packet_is_newer = self
            .last_media_seq
            .is_none_or(|last_seq| sequence_is_newer(pkt.seq, last_seq));
        let start_after_lost_end =
            explicit_start && self.last_media_seq.is_some() && packet_is_newer;
        let timestamp_discontinuity =
            !pkt.payload.is_empty() && packet_is_newer && self.media_timestamp_has_dtx_gap(&pkt);
        let implicit_start = !pkt.payload.is_empty()
            && self
                .end_of_talkspurt_seq
                .is_some_and(|end_seq| sequence_is_newer(pkt.seq, end_seq));
        if ((explicit_start || implicit_start)
            && (self.pending_silence || self.intentional_silence))
            || start_after_lost_end
            || timestamp_discontinuity
        {
            self.inner.flush();
            self.pending_silence = false;
            self.intentional_silence = false;
            self.end_of_talkspurt_seq = None;
            self.resuming_talkspurt = true;
            self.resumed_packet_count = 0;
        }

        if packet_is_newer
            && pkt
                .flags
                .contains(crate::packet::PacketFlags::END_OF_TALKSPURT)
        {
            self.pending_silence = true;
            self.end_of_talkspurt_seq = Some(pkt.seq);
        }

        if pkt.payload.is_empty() {
            return Ok(());
        }

        let header = RtpHeader::new(pkt.seq, pkt.timestamp, SSRC_FIXED, PAYLOAD_TYPE_OPUS, false);
        let mut packet = AudioPacket::new(header, pkt.payload, self.sample_rate, self.channels, 20);
        packet.arrival_time =
            self.instant_for_arrival(arrival.received_at_mono_us, simulated_now_mono_us);

        self.inner.insert_packet(packet).map_err(|err| {
            self.record_failure(format!("insert_packet: {err}"));
            Error::from(err)
        })?;
        if packet_is_newer {
            self.last_media_seq = Some(pkt.seq);
            self.last_media_timestamp = Some(pkt.timestamp);
        }
        if self.resuming_talkspurt {
            self.resumed_packet_count = self.resumed_packet_count.saturating_add(1);
        }

        Ok(())
    }

    pub fn pull_frame(&mut self, out: &mut [f32]) {
        if self.pending_silence {
            let stats = self.inner.get_statistics();
            // NetEq's integer delay estimate can settle one millisecond above a
            // nominal 10 ms frame. Do not turn that rounding residue into
            // unbounded PLC after an explicit end marker.
            if stats.current_buffer_size_ms
                <= OUTPUT_FRAME_MS + TALKSPURT_DRAIN_ROUNDING_TOLERANCE_MS
                && stats.packets_awaiting_decode == 0
            {
                self.pending_silence = false;
                self.intentional_silence = true;
            }
        }

        if self.intentional_silence {
            out.fill(0.0);
            self.consecutive_failures = 0;
            return;
        }

        if self.resuming_talkspurt {
            if self.resumed_packet_count < TALKSPURT_RESUME_PREBUFFER_PACKETS {
                out.fill(0.0);
                self.consecutive_failures = 0;
                return;
            }
            self.resuming_talkspurt = false;
        }

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
            preferred_buffer_size_ms: stats.network.preferred_buffer_size_ms as u32,
            packets_awaiting_decode: stats.packets_awaiting_decode,
            packets_per_sec: stats.packets_per_sec,
            expand_rate: stats.network.expand_rate,
            accelerate_rate: stats.network.accelerate_rate,
            preemptive_rate: stats.network.preemptive_rate,
            expand_per_sec: stats.network.operation_counters.expand_per_sec,
            accelerate_per_sec: stats.network.operation_counters.accelerate_per_sec
                + stats.network.operation_counters.fast_accelerate_per_sec,
            preemptive_expand_per_sec: stats.network.operation_counters.preemptive_expand_per_sec,
            normal_per_sec: stats.network.operation_counters.normal_per_sec,
            concealed_samples: stats.lifetime.concealed_samples,
            concealment_events: stats.lifetime.concealment_events,
            silent_concealed_samples: stats.lifetime.silent_concealed_samples,
            late_packets_discarded: stats.lifetime.late_packets_discarded,
            inserted_samples_for_deceleration: stats.lifetime.inserted_samples_for_deceleration,
            removed_samples_for_acceleration: stats.lifetime.removed_samples_for_acceleration,
            consecutive_failures: self.consecutive_failures,
            sticky_error: self.sticky_error.clone(),
            intentional_silence: self.intentional_silence || self.pending_silence,
            playout_paused: self.intentional_silence,
        }
    }

    /// Discard buffered media and decoder history before assigning this
    /// receiver to a new logical stream.
    pub fn reset_stream(&mut self) -> Result<()> {
        self.inner.flush();
        self.inner.reset_decoders()?;
        self.pending_silence = false;
        self.intentional_silence = false;
        self.end_of_talkspurt_seq = None;
        self.resuming_talkspurt = false;
        self.resumed_packet_count = 0;
        self.last_media_seq = None;
        self.last_media_timestamp = None;
        self.consecutive_failures = 0;
        Ok(())
    }

    fn instant_for_arrival(
        &mut self,
        received_at_mono_us: u64,
        simulated_now_mono_us: u64,
    ) -> Instant {
        let now = Instant::now();
        if received_at_mono_us <= simulated_now_mono_us {
            now.checked_sub(Duration::from_micros(
                simulated_now_mono_us - received_at_mono_us,
            ))
            .unwrap_or(now)
        } else {
            now.checked_add(Duration::from_micros(
                received_at_mono_us - simulated_now_mono_us,
            ))
            .unwrap_or(now)
        }
    }

    fn media_timestamp_has_dtx_gap(&self, pkt: &VoicePacket) -> bool {
        let (Some(last_seq), Some(last_timestamp)) =
            (self.last_media_seq, self.last_media_timestamp)
        else {
            return false;
        };
        let sequence_frames = u32::from(pkt.seq.wrapping_sub(last_seq));
        if sequence_frames == 0 || sequence_frames >= (1 << 15) {
            return false;
        }
        let expected_samples = sequence_frames.wrapping_mul(OPUS_PACKET_SAMPLES);
        let actual_samples = pkt.timestamp.wrapping_sub(last_timestamp);
        actual_samples.saturating_sub(expected_samples)
            >= DTX_DISCONTINUITY_MIN_MISSING_FRAMES * OPUS_PACKET_SAMPLES
    }

    fn record_failure(&mut self, message: String) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.sticky_error.is_none() {
            self.sticky_error = Some(message);
        }
    }
}

fn sequence_is_newer(sequence: u16, reference: u16) -> bool {
    let distance = sequence.wrapping_sub(reference);
    distance != 0 && distance < (1 << 15)
}

#[cfg(test)]
mod tests {
    use anyhow::Result as AnyResult;

    use super::*;
    use crate::encoder::{VoiceEncoder, VoiceEncoderConfig};
    use crate::fixture_harness::{
        default_profile, rms, run_impairment_harness, ENCODE_FRAME_SAMPLES, PULL_FRAME_SAMPLES,
        SAMPLE_RATE,
    };

    #[test]
    fn encode_decode_sine_through_receiver_keeps_rms_reasonable() -> AnyResult<()> {
        let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;

        let input = sine_wave_seconds(1.0, 440.0, SAMPLE_RATE, 0.2);
        let input_rms = rms(&input);

        let mut output = Vec::new();
        let mut arrival_us = 0_u64;

        for chunk in input.chunks(ENCODE_FRAME_SAMPLES) {
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
                let mut frame = vec![0.0; PULL_FRAME_SAMPLES];
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

    #[test]
    fn deterministic_impairment_profile_stays_alive() -> AnyResult<()> {
        let input = synthetic_speech_like_seconds(3.0, SAMPLE_RATE);
        let run = run_impairment_harness(&input, &default_profile())?;
        let output_rms = rms(&run.output_samples);
        let stats = run.final_stats;
        let metrics = run.metrics;

        assert!(output_rms > 0.01, "output unexpectedly close to silence");
        assert_eq!(stats.consecutive_failures, 0);
        assert!(stats.sticky_error.is_none());
        assert!(metrics.measured_frames > 0);
        assert!(metrics.measured_output_rms > 0.01);

        Ok(())
    }

    #[test]
    fn end_of_talkspurt_enters_intentional_silence() -> AnyResult<()> {
        let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;

        encoder.push_pcm(&vec![0.1; ENCODE_FRAME_SAMPLES]);
        let start_pkt = encoder.poll_packet()?.expect("expected voiced packet");
        assert!(start_pkt
            .flags
            .contains(crate::packet::PacketFlags::START_OF_TALKSPURT));

        let end_pkt = VoicePacket {
            seq: start_pkt.seq.wrapping_add(1),
            timestamp: start_pkt
                .timestamp
                .wrapping_add(ENCODE_FRAME_SAMPLES as u32),
            flags: crate::packet::PacketFlags::from_bits(
                crate::packet::PacketFlags::END_OF_TALKSPURT,
            ),
            payload: Vec::new(),
        };
        assert!(end_pkt.payload.is_empty());

        receiver.push_packet(
            start_pkt,
            PacketArrival {
                received_at_mono_us: 0,
            },
        )?;
        receiver.push_packet(
            end_pkt,
            PacketArrival {
                received_at_mono_us: 20_000,
            },
        )?;

        for _ in 0..12 {
            let mut frame = vec![0.0; PULL_FRAME_SAMPLES];
            receiver.pull_frame(&mut frame);
            if receiver.stats().intentional_silence {
                break;
            }
        }

        let stats_before = receiver.stats();
        assert!(stats_before.intentional_silence);
        let concealed_before = stats_before.concealed_samples;

        let mut silent_frame = vec![1.0; PULL_FRAME_SAMPLES];
        receiver.pull_frame(&mut silent_frame);
        receiver.pull_frame(&mut silent_frame);

        let stats_after = receiver.stats();
        assert!(
            stats_after
                .concealed_samples
                .saturating_sub(concealed_before)
                <= PULL_FRAME_SAMPLES as u64 * 2,
            "concealment kept growing after intentional silence"
        );
        assert!(stats_after.intentional_silence);

        Ok(())
    }

    #[test]
    fn long_talkspurt_does_not_stick_in_concealment_at_eleven_ms() -> AnyResult<()> {
        let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        let mut receiver = VoiceReceiver::new_with_delay_bounds(SAMPLE_RATE, 80, 120)?;
        let mut now_us = 0_u64;

        for _ in 0..150 {
            encoder.push_pcm(&[0.1; ENCODE_FRAME_SAMPLES]);
            if let Some(packet) = encoder.poll_packet()? {
                receiver.push_packet_with_now_mono(
                    packet,
                    PacketArrival {
                        received_at_mono_us: now_us,
                    },
                    now_us,
                )?;
            }
            for _ in 0..2 {
                receiver.pull_frame(&mut [0.0; PULL_FRAME_SAMPLES]);
            }
            now_us += 20_000;
        }

        let mut saw_end = false;
        for _ in 0..50 {
            encoder.push_pcm(&[0.0; ENCODE_FRAME_SAMPLES]);
            if let Some(packet) = encoder.poll_packet()? {
                saw_end |= packet
                    .flags
                    .contains(crate::packet::PacketFlags::END_OF_TALKSPURT);
                receiver.push_packet_with_now_mono(
                    packet,
                    PacketArrival {
                        received_at_mono_us: now_us,
                    },
                    now_us,
                )?;
            }
            for _ in 0..2 {
                receiver.pull_frame(&mut [0.0; PULL_FRAME_SAMPLES]);
            }
            now_us += 20_000;
            if saw_end && receiver.stats().intentional_silence {
                break;
            }
        }

        assert!(saw_end, "Opus DTX never emitted an end marker");
        assert!(
            receiver.stats().intentional_silence,
            "receiver kept treating an 11 ms drain residue as missing media"
        );
        Ok(())
    }

    #[test]
    fn nonempty_packet_resumes_when_start_marker_was_lost() -> AnyResult<()> {
        let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;

        encoder.push_pcm(&vec![0.1; ENCODE_FRAME_SAMPLES]);
        let start = encoder.poll_packet()?.expect("expected voiced packet");
        let end = VoicePacket {
            seq: start.seq.wrapping_add(1),
            timestamp: start.timestamp.wrapping_add(ENCODE_FRAME_SAMPLES as u32),
            flags: crate::packet::PacketFlags::from_bits(
                crate::packet::PacketFlags::END_OF_TALKSPURT,
            ),
            payload: Vec::new(),
        };
        receiver.push_packet(
            start,
            PacketArrival {
                received_at_mono_us: 0,
            },
        )?;
        receiver.push_packet(
            end.clone(),
            PacketArrival {
                received_at_mono_us: 20_000,
            },
        )?;
        for _ in 0..12 {
            receiver.pull_frame(&mut vec![0.0; PULL_FRAME_SAMPLES]);
        }
        assert!(receiver.stats().intentional_silence);

        // Model a lost START packet by sending only later, unflagged packets
        // from the new talkspurt. Two packets satisfy the normal resume prebuffer.
        for offset in 2..=3 {
            encoder.push_pcm(&vec![0.1; ENCODE_FRAME_SAMPLES]);
            let mut packet = encoder.poll_packet()?.expect("expected voiced packet");
            packet.seq = end.seq.wrapping_add(offset);
            packet.timestamp = end
                .timestamp
                .wrapping_add(offset as u32 * ENCODE_FRAME_SAMPLES as u32);
            packet.flags = crate::packet::PacketFlags::default();
            receiver.push_packet(
                packet,
                PacketArrival {
                    received_at_mono_us: offset as u64 * 20_000,
                },
            )?;
        }

        assert!(!receiver.stats().intentional_silence);
        Ok(())
    }

    #[test]
    fn explicit_start_recovers_when_end_marker_was_lost() -> AnyResult<()> {
        let (start, _lost_end, resumed) = dtx_packet_sequence()?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;
        receiver.push_packet(
            start,
            PacketArrival {
                received_at_mono_us: 0,
            },
        )?;

        receiver.push_packet(
            resumed[0].clone(),
            PacketArrival {
                received_at_mono_us: 2_000_000,
            },
        )?;
        assert!(receiver.resuming_talkspurt);
        assert_eq!(receiver.resumed_packet_count, 1);
        receiver.push_packet(
            resumed[1].clone(),
            PacketArrival {
                received_at_mono_us: 2_020_000,
            },
        )?;
        assert_eq!(receiver.resumed_packet_count, 2);
        Ok(())
    }

    #[test]
    fn timestamp_gap_recovers_when_both_markers_were_lost() -> AnyResult<()> {
        let (start, _lost_end, resumed) = dtx_packet_sequence()?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;
        receiver.push_packet(
            start,
            PacketArrival {
                received_at_mono_us: 0,
            },
        )?;

        // Drop EOT and the flagged first resume packet. The second packet has a
        // large media-timestamp gap but only a small transmitted-sequence gap.
        let mut unflagged_resume = resumed[1].clone();
        unflagged_resume.flags = crate::packet::PacketFlags::default();
        receiver.push_packet(
            unflagged_resume,
            PacketArrival {
                received_at_mono_us: 2_020_000,
            },
        )?;
        assert!(receiver.resuming_talkspurt);
        assert_eq!(receiver.resumed_packet_count, 1);
        Ok(())
    }

    #[test]
    fn reordered_stale_end_does_not_stop_new_talkspurt() -> AnyResult<()> {
        let (start, stale_end, resumed) = dtx_packet_sequence()?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;
        receiver.push_packet(
            start,
            PacketArrival {
                received_at_mono_us: 0,
            },
        )?;
        for (index, packet) in resumed.into_iter().enumerate() {
            receiver.push_packet(
                packet,
                PacketArrival {
                    received_at_mono_us: 2_000_000 + index as u64 * 20_000,
                },
            )?;
        }
        receiver.push_packet(
            stale_end,
            PacketArrival {
                received_at_mono_us: 2_040_000,
            },
        )?;

        assert!(!receiver.pending_silence);
        assert!(!receiver.intentional_silence);
        Ok(())
    }

    #[test]
    fn sequence_newer_handles_wrap_and_rejects_old_packets() {
        assert!(sequence_is_newer(0, u16::MAX));
        assert!(sequence_is_newer(11, 10));
        assert!(!sequence_is_newer(10, 10));
        assert!(!sequence_is_newer(9, 10));
    }

    #[test]
    fn reset_stream_accepts_a_new_opus_sequence() -> AnyResult<()> {
        let mut first_encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        let mut receiver = VoiceReceiver::new(SAMPLE_RATE)?;

        first_encoder.push_pcm(&vec![0.1; ENCODE_FRAME_SAMPLES]);
        let first = first_encoder.poll_packet()?.expect("first packet");
        receiver.push_packet(
            first,
            PacketArrival {
                received_at_mono_us: 0,
            },
        )?;
        receiver.reset_stream()?;
        assert_eq!(receiver.stats().current_buffer_size_ms, 0);

        let mut second_encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        second_encoder.push_pcm(&vec![0.1; ENCODE_FRAME_SAMPLES]);
        let second = second_encoder.poll_packet()?.expect("second packet");
        receiver.push_packet(
            second,
            PacketArrival {
                received_at_mono_us: 20_000,
            },
        )?;
        let mut frame = vec![0.0; PULL_FRAME_SAMPLES];
        receiver.pull_frame(&mut frame);
        assert!(receiver.stats().sticky_error.is_none());
        Ok(())
    }

    fn dtx_packet_sequence() -> AnyResult<(VoicePacket, VoicePacket, [VoicePacket; 2])> {
        let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
        encoder.push_pcm(&[0.1; ENCODE_FRAME_SAMPLES]);
        let start = encoder.poll_packet()?.expect("initial voice packet");

        let mut end = None;
        for _ in 0..200 {
            encoder.push_pcm(&[0.0; ENCODE_FRAME_SAMPLES]);
            if let Some(packet) = encoder.poll_packet()? {
                if packet
                    .flags
                    .contains(crate::packet::PacketFlags::END_OF_TALKSPURT)
                {
                    end = Some(packet);
                    break;
                }
            }
        }
        let end = end.expect("Opus DTX should end the talkspurt");

        // Keep the media clock moving through a long no-send period.
        for _ in 0..50 {
            encoder.push_pcm(&[0.0; ENCODE_FRAME_SAMPLES]);
            let _ = encoder.poll_packet()?;
        }

        let mut resumed = Vec::new();
        for _ in 0..20 {
            encoder.push_pcm(&[0.1; ENCODE_FRAME_SAMPLES]);
            if let Some(packet) = encoder.poll_packet()? {
                resumed.push(packet);
                if resumed.len() == 2 {
                    break;
                }
            }
        }
        let resumed: [VoicePacket; 2] = resumed
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected two resumed voice packets"))?;
        assert!(resumed[0]
            .flags
            .contains(crate::packet::PacketFlags::START_OF_TALKSPURT));
        Ok((start, end, resumed))
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

    fn synthetic_speech_like_seconds(seconds: f32, sample_rate: u32) -> Vec<f32> {
        let total_samples = (seconds * sample_rate as f32) as usize;
        (0..total_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let syllable_env =
                    ((2.0 * std::f32::consts::PI * 3.2 * t).sin() * 0.5 + 0.5).powf(1.5);
                let voiced = (2.0 * std::f32::consts::PI * 180.0 * t).sin()
                    + 0.35 * (2.0 * std::f32::consts::PI * 720.0 * t).sin()
                    + 0.2 * (2.0 * std::f32::consts::PI * 1260.0 * t).sin();
                let fricative = if (t * 7.0).sin() > 0.65 {
                    (((i as u32).wrapping_mul(1103515245).wrapping_add(12345) >> 8) as f32
                        / u32::MAX as f32)
                        * 2.0
                        - 1.0
                } else {
                    0.0
                };

                (0.07 * voiced + 0.015 * fricative) * syllable_env
            })
            .collect()
    }
}

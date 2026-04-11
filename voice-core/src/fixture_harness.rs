use crate::encoder::{VoiceEncoder, VoiceEncoderConfig};
use crate::packet::{PacketArrival, VoicePacket};
use crate::receiver::{ReceiverStats, VoiceReceiver};
use crate::Result;

pub const ENCODE_FRAME_SAMPLES: usize = 960;
pub const PULL_FRAME_SAMPLES: usize = 480;
pub const SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone)]
pub struct ScheduledPacket {
    pub packet: VoicePacket,
    pub arrival_us: u64,
}

#[derive(Debug, Clone)]
pub struct HarnessOutput {
    pub input_samples: Vec<f32>,
    pub output_samples: Vec<f32>,
    pub final_stats: ReceiverStats,
    pub metrics: HarnessMetrics,
}

#[derive(Debug, Clone)]
pub struct HarnessMetrics {
    pub startup_buffer_ms: u32,
    pub warmup_ms: u32,
    pub tail_trim_ms: u32,
    pub measured_frames: usize,
    pub measured_output_rms: f32,
    pub concealed_samples_delta: u64,
    pub avg_preferred_buffer_size_ms: f32,
    pub avg_current_buffer_size_ms: f32,
    pub avg_target_delay_ms: f32,
    pub avg_expand_rate_q14: f32,
    pub avg_accelerate_rate_q14: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct HarnessConfig {
    pub startup_buffer_ms: u32,
    pub warmup_ms: u32,
    pub tail_trim_ms: u32,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            startup_buffer_ms: 60,
            warmup_ms: 400,
            tail_trim_ms: 200,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImpairmentProfile {
    pub name: &'static str,
    pub jitter_pattern_us: &'static [u64],
    pub drop_every_nth: Option<usize>,
    pub drop_offset: usize,
    pub burst_loss_ranges: &'static [(usize, usize)],
}

impl ImpairmentProfile {
    pub fn clean() -> Self {
        Self {
            name: "clean",
            jitter_pattern_us: &[0],
            drop_every_nth: None,
            drop_offset: 0,
            burst_loss_ranges: &[],
        }
    }

    pub fn good_network() -> Self {
        Self {
            name: "good_network",
            jitter_pattern_us: &[0, 600, 1_200, 400, 1_800, 0, 900, 300],
            drop_every_nth: None,
            drop_offset: 0,
            burst_loss_ranges: &[],
        }
    }

    pub fn normal_wan() -> Self {
        Self {
            name: "normal_wan",
            jitter_pattern_us: &[0, 1_500, 3_000, 800, 4_500, 2_000, 0, 2_500],
            drop_every_nth: Some(181),
            drop_offset: 37,
            burst_loss_ranges: &[(90, 91)],
        }
    }

    pub fn bad_wan() -> Self {
        Self {
            name: "bad_wan",
            jitter_pattern_us: &[0, 4_000, 1_500, 9_000, 2_500, 6_000, 700, 5_000],
            drop_every_nth: Some(71),
            drop_offset: 9,
            burst_loss_ranges: &[(60, 62)],
        }
    }

    pub fn stress() -> Self {
        Self {
            name: "stress",
            jitter_pattern_us: &[0, 9_000, 4_000, 18_000, 6_000, 15_000, 2_000, 12_000],
            drop_every_nth: Some(23),
            drop_offset: 7,
            burst_loss_ranges: &[(45, 49), (90, 94)],
        }
    }
}

pub fn default_profile() -> ImpairmentProfile {
    ImpairmentProfile::normal_wan()
}

pub fn sweep_profiles() -> Vec<ImpairmentProfile> {
    vec![
        ImpairmentProfile::clean(),
        ImpairmentProfile::good_network(),
        ImpairmentProfile::normal_wan(),
        ImpairmentProfile::bad_wan(),
        ImpairmentProfile::stress(),
    ]
}

pub fn run_impairment_harness(
    input_48k_mono: &[f32],
    profile: &ImpairmentProfile,
) -> Result<HarnessOutput> {
    run_impairment_harness_with_config(input_48k_mono, profile, 20, 250, HarnessConfig::default())
}

pub fn run_impairment_harness_with_delay_bounds(
    input_48k_mono: &[f32],
    profile: &ImpairmentProfile,
    min_delay_ms: u32,
    max_delay_ms: u32,
) -> Result<HarnessOutput> {
    run_impairment_harness_with_config(
        input_48k_mono,
        profile,
        min_delay_ms,
        max_delay_ms,
        HarnessConfig::default(),
    )
}

pub fn run_impairment_harness_with_config(
    input_48k_mono: &[f32],
    profile: &ImpairmentProfile,
    min_delay_ms: u32,
    max_delay_ms: u32,
    config: HarnessConfig,
) -> Result<HarnessOutput> {
    let packets = encode_packets(input_48k_mono)?;
    let events = impair_packets(&packets, profile);
    let mut output = Vec::new();
    let mut next_event = 0_usize;
    let first_arrival_us = events.first().map(|event| event.arrival_us).unwrap_or(0);
    let last_arrival_us = events.last().map(|event| event.arrival_us).unwrap_or(0);
    let end_time_us = last_arrival_us + 200_000;
    let playout_start_us = first_arrival_us + config.startup_buffer_ms as u64 * 1_000;
    let measure_start_us = playout_start_us + config.warmup_ms as u64 * 1_000;
    let measure_end_us = end_time_us.saturating_sub(config.tail_trim_ms as u64 * 1_000);
    let mut receiver =
        VoiceReceiver::new_with_delay_bounds(SAMPLE_RATE, min_delay_ms, max_delay_ms)?;
    let mut t = 0_u64;
    let mut measurement_started = false;
    let mut concealed_start = 0_u64;
    let mut measured_samples = Vec::new();
    let mut measured_frames = 0_usize;
    let mut sum_preferred_buffer_size_ms = 0.0_f32;
    let mut sum_current_buffer_size_ms = 0.0_f32;
    let mut sum_target_delay_ms = 0.0_f32;
    let mut sum_expand_rate_q14 = 0.0_f32;
    let mut sum_accelerate_rate_q14 = 0.0_f32;

    while t <= end_time_us {
        while next_event < events.len() && events[next_event].arrival_us <= t {
            let event = &events[next_event];
            receiver.push_packet_with_simulated_now(
                event.packet.clone(),
                PacketArrival {
                    received_at_mono_us: event.arrival_us,
                },
                t,
            )?;
            next_event += 1;
        }

        let mut frame = vec![0.0; PULL_FRAME_SAMPLES];
        if t >= playout_start_us {
            receiver.pull_frame(&mut frame);
        }
        output.extend_from_slice(&frame);

        if t >= measure_start_us && t <= measure_end_us {
            let stats = receiver.stats();
            if !measurement_started {
                concealed_start = stats.concealed_samples;
                measurement_started = true;
            }

            measured_samples.extend_from_slice(&frame);
            measured_frames += 1;
            sum_preferred_buffer_size_ms += stats.preferred_buffer_size_ms as f32;
            sum_current_buffer_size_ms += stats.current_buffer_size_ms as f32;
            sum_target_delay_ms += stats.target_delay_ms as f32;
            sum_expand_rate_q14 += stats.expand_rate as f32;
            sum_accelerate_rate_q14 += stats.accelerate_rate as f32;
        }
        t += 10_000;
    }

    let final_stats = receiver.stats();
    let measured_output_rms = if measured_samples.is_empty() {
        0.0
    } else {
        rms(&measured_samples)
    };
    let denom = measured_frames.max(1) as f32;
    let metrics = HarnessMetrics {
        startup_buffer_ms: config.startup_buffer_ms,
        warmup_ms: config.warmup_ms,
        tail_trim_ms: config.tail_trim_ms,
        measured_frames,
        measured_output_rms,
        concealed_samples_delta: final_stats
            .concealed_samples
            .saturating_sub(concealed_start),
        avg_preferred_buffer_size_ms: sum_preferred_buffer_size_ms / denom,
        avg_current_buffer_size_ms: sum_current_buffer_size_ms / denom,
        avg_target_delay_ms: sum_target_delay_ms / denom,
        avg_expand_rate_q14: sum_expand_rate_q14 / denom,
        avg_accelerate_rate_q14: sum_accelerate_rate_q14 / denom,
    };

    Ok(HarnessOutput {
        input_samples: input_48k_mono.to_vec(),
        output_samples: output,
        final_stats,
        metrics,
    })
}

pub fn encode_packets(input: &[f32]) -> Result<Vec<VoicePacket>> {
    let mut encoder = VoiceEncoder::new(VoiceEncoderConfig::default())?;
    let mut packets = Vec::new();

    for chunk in input.chunks(ENCODE_FRAME_SAMPLES) {
        encoder.push_pcm(chunk);
        if let Some(packet) = encoder.poll_packet()? {
            packets.push(packet);
        }
    }

    Ok(packets)
}

pub fn impair_packets(
    packets: &[VoicePacket],
    profile: &ImpairmentProfile,
) -> Vec<ScheduledPacket> {
    let mut events = Vec::new();

    for (i, packet) in packets.iter().enumerate() {
        if should_drop_packet(i, profile) {
            continue;
        }

        let jitter_pattern = profile.jitter_pattern_us;
        let arrival_us = i as u64 * 20_000 + jitter_pattern[i % jitter_pattern.len()];
        events.push(ScheduledPacket {
            packet: packet.clone(),
            arrival_us,
        });
    }

    events.sort_by_key(|event| event.arrival_us);
    events
}

fn should_drop_packet(index: usize, profile: &ImpairmentProfile) -> bool {
    if let Some(n) = profile.drop_every_nth {
        if index % n == profile.drop_offset {
            return true;
        }
    }

    profile
        .burst_loss_ranges
        .iter()
        .any(|(start, end)| (*start..=*end).contains(&index))
}

pub fn rms(samples: &[f32]) -> f32 {
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

pub fn linear_resample_mono(input: &[f32], input_rate: u32, output_rate: u32) -> Vec<f32> {
    if input_rate == output_rate || input.is_empty() {
        return input.to_vec();
    }

    let out_len =
        ((input.len() as u64 * output_rate as u64) + input_rate as u64 - 1) / input_rate as u64;
    let out_len = out_len as usize;
    let ratio = input_rate as f64 / output_rate as f64;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = input[idx.min(input.len() - 1)];
        let b = input[(idx + 1).min(input.len() - 1)];
        out.push(a + (b - a) * frac);
    }

    out
}

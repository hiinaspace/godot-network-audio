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
    pub stats: ReceiverStats,
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
    pub fn mild_wan() -> Self {
        Self {
            name: "mild_wan",
            jitter_pattern_us: &[0, 3_000, 1_000, 6_000, 2_000, 4_000, 0, 5_000],
            drop_every_nth: Some(71),
            drop_offset: 9,
            burst_loss_ranges: &[],
        }
    }

    pub fn moderate_wan() -> Self {
        Self {
            name: "moderate_wan",
            jitter_pattern_us: &[0, 7_000, 2_000, 16_000, 4_000, 11_000, 1_000, 9_000],
            drop_every_nth: Some(37),
            drop_offset: 11,
            burst_loss_ranges: &[(60, 63)],
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
    ImpairmentProfile::moderate_wan()
}

pub fn sweep_profiles() -> Vec<ImpairmentProfile> {
    vec![
        ImpairmentProfile::mild_wan(),
        ImpairmentProfile::moderate_wan(),
        ImpairmentProfile::stress(),
    ]
}

pub fn run_impairment_harness(
    input_48k_mono: &[f32],
    profile: &ImpairmentProfile,
) -> Result<HarnessOutput> {
    run_impairment_harness_with_delay_bounds(input_48k_mono, profile, 20, 250)
}

pub fn run_impairment_harness_with_delay_bounds(
    input_48k_mono: &[f32],
    profile: &ImpairmentProfile,
    min_delay_ms: u32,
    max_delay_ms: u32,
) -> Result<HarnessOutput> {
    let packets = encode_packets(input_48k_mono)?;
    let events = impair_packets(&packets, profile);

    let mut receiver =
        VoiceReceiver::new_with_delay_bounds(SAMPLE_RATE, min_delay_ms, max_delay_ms)?;
    let mut output = Vec::new();
    let mut next_event = 0_usize;
    let last_arrival_us = events.last().map(|event| event.arrival_us).unwrap_or(0);
    let end_time_us = last_arrival_us + 200_000;
    let mut t = 0_u64;

    while t <= end_time_us {
        while next_event < events.len() && events[next_event].arrival_us <= t {
            let event = &events[next_event];
            receiver.push_packet(
                event.packet.clone(),
                PacketArrival {
                    received_at_mono_us: event.arrival_us,
                },
            )?;
            next_event += 1;
        }

        let mut frame = vec![0.0; PULL_FRAME_SAMPLES];
        receiver.pull_frame(&mut frame);
        output.extend_from_slice(&frame);
        t += 10_000;
    }

    let stats = receiver.stats();
    Ok(HarnessOutput {
        input_samples: input_48k_mono.to_vec(),
        output_samples: output,
        stats,
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

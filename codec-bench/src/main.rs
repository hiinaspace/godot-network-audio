use std::f32::consts::TAU;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use shiguredo_opus::{
    Application, Decoder, DecoderConfig, Dred, DredDecoder, Encoder, EncoderConfig, FrameDuration,
};

const SAMPLE_RATE: usize = 48_000;
const FRAME_SAMPLES: usize = 960;
const FRAME_MS: usize = 20;
const SIGNAL_FRAMES: usize = 200;

fn main() -> Result<()> {
    let frames = std::env::args()
        .nth(1)
        .map(|value| value.parse())
        .transpose()
        .context("invalid frame count")?
        .unwrap_or(10_000);
    run_dred(frames)
}

fn run_dred(frames: usize) -> Result<()> {
    let signal = signal_frames();
    for bitrate in [16_000_u32, 24_000, 32_000, 48_000, 64_000] {
        let disabled = measure_encoder(&signal, frames, bitrate, None)?;
        let enabled = measure_encoder(&signal, frames, bitrate, Some(100))?;
        let recovery = dred_recovery_smoke(&signal, bitrate)?;

        println!(
            "{}",
            json!({
                "benchmark": "opus_1_6_dred",
                "opus_version": shiguredo_opus::version_string(),
                "bitrate_bps": bitrate,
                "frames": frames,
                "dred_duration_10ms": 100,
                "disabled": disabled,
                "enabled": enabled,
                "payload_overhead_ratio": enabled.payload_bytes as f64 / disabled.payload_bytes as f64,
                "cpu_ratio": enabled.cpu_seconds / disabled.cpu_seconds,
                "recovery": recovery,
            })
        );
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct EncodeMeasurement {
    payload_bytes: u64,
    payload_bitrate_bps: f64,
    cpu_seconds: f64,
    wall_seconds: f64,
    cpu_ns_per_frame: f64,
    min_packet_bytes: usize,
    max_packet_bytes: usize,
}

fn measure_encoder(
    signal: &[Vec<f32>],
    frames: usize,
    bitrate: u32,
    dred_duration: Option<u32>,
) -> Result<EncodeMeasurement> {
    let mut encoder = new_encoder(bitrate, dred_duration)?;
    for i in 0..500 {
        encoder.encode_f32(&signal[i % signal.len()])?;
    }

    let cpu_start = process_cpu_seconds()?;
    let wall_start = Instant::now();
    let mut payload_bytes = 0_u64;
    let mut min_packet_bytes = usize::MAX;
    let mut max_packet_bytes = 0;
    for i in 0..frames {
        let packet = encoder.encode_f32(&signal[i % signal.len()])?;
        payload_bytes += packet.len() as u64;
        min_packet_bytes = min_packet_bytes.min(packet.len());
        max_packet_bytes = max_packet_bytes.max(packet.len());
    }
    let wall_seconds = wall_start.elapsed().as_secs_f64();
    let cpu_seconds = process_cpu_seconds()? - cpu_start;
    let simulated_seconds = frames as f64 * FRAME_MS as f64 / 1_000.0;
    Ok(EncodeMeasurement {
        payload_bytes,
        payload_bitrate_bps: payload_bytes as f64 * 8.0 / simulated_seconds,
        cpu_seconds,
        wall_seconds,
        cpu_ns_per_frame: cpu_seconds * 1e9 / frames as f64,
        min_packet_bytes,
        max_packet_bytes,
    })
}

fn new_encoder(bitrate: u32, dred_duration: Option<u32>) -> Result<Encoder> {
    let mut config = EncoderConfig::new(SAMPLE_RATE as u32, 1);
    config.bitrate = Some(bitrate);
    config.application = Some(Application::Voip);
    config.frame_duration = Some(FrameDuration::Ms20);
    config.vbr = Some(true);
    config.dtx = Some(false);
    // DRED only allocates redundancy when the encoder is told to expect loss.
    // Keep this equal for enabled and disabled measurements.
    config.packet_loss_perc = Some(10);
    config.dred_duration = dred_duration;
    Ok(Encoder::new(config)?)
}

#[derive(serde::Serialize)]
struct RecoveryMeasurement {
    lost_frames: usize,
    available_dred_samples: i32,
    fully_dred_covered_frames: usize,
    decoded_gap_frames: usize,
    decoded_gap_rms: Vec<f64>,
    disabled_offset_samples: i32,
}

fn dred_recovery_smoke(signal: &[Vec<f32>], bitrate: u32) -> Result<RecoveryMeasurement> {
    const LOST_FRAMES: usize = 5;
    const FIRST_LOST: usize = 100;
    const FIRST_RECEIVED: usize = FIRST_LOST + LOST_FRAMES;

    let mut enabled = new_encoder(bitrate, Some(100))?;
    let mut disabled = new_encoder(bitrate, None)?;
    let mut enabled_packets = Vec::with_capacity(FIRST_RECEIVED + 1);
    let mut disabled_packet = Vec::new();
    for i in 0..=FIRST_RECEIVED {
        enabled_packets.push(enabled.encode_f32(&signal[i % signal.len()])?);
        disabled_packet = disabled.encode_f32(&signal[i % signal.len()])?;
    }

    let decoder_config = DecoderConfig {
        frame_duration: Some(FrameDuration::Ms20),
        ..DecoderConfig::new(SAMPLE_RATE as u32, 1)
    };
    let mut decoder = Decoder::new(decoder_config)?;
    for packet in &enabled_packets[..FIRST_LOST] {
        decoder.decode_f32(packet)?;
    }

    let mut dred_decoder = DredDecoder::new()?;
    let mut dred = Dred::new()?;
    let parsed_offset = dred_decoder.parse(
        &mut dred,
        &enabled_packets[FIRST_RECEIVED],
        (LOST_FRAMES * FRAME_SAMPLES) as i32,
        SAMPLE_RATE as i32,
    )?;
    let mut decoded_gap_rms = Vec::new();
    if parsed_offset > 0 {
        for frame in 0..LOST_FRAMES {
            let offset = ((LOST_FRAMES - frame) * FRAME_SAMPLES) as i32;
            let recovered = decoder.dred_decode_f32(&dred, offset)?;
            decoded_gap_rms.push(rms(&recovered));
        }
    }

    let mut disabled_dred = Dred::new()?;
    let disabled_offset = dred_decoder.parse(
        &mut disabled_dred,
        &disabled_packet,
        (LOST_FRAMES * FRAME_SAMPLES) as i32,
        SAMPLE_RATE as i32,
    )?;

    Ok(RecoveryMeasurement {
        lost_frames: LOST_FRAMES,
        available_dred_samples: parsed_offset,
        fully_dred_covered_frames: (parsed_offset.max(0) as usize / FRAME_SAMPLES).min(LOST_FRAMES),
        decoded_gap_frames: decoded_gap_rms.len(),
        decoded_gap_rms,
        disabled_offset_samples: disabled_offset,
    })
}

fn signal_frames() -> Vec<Vec<f32>> {
    (0..SIGNAL_FRAMES)
        .map(|frame| {
            (0..FRAME_SAMPLES)
                .map(|sample| {
                    let index = frame * FRAME_SAMPLES + sample;
                    let time = index as f32 / SAMPLE_RATE as f32;
                    let envelope = 0.18 + 0.08 * (TAU * 2.3 * time).sin().abs();
                    envelope
                        * ((TAU * 173.0 * time).sin()
                            + 0.45 * (TAU * 317.0 * time).sin()
                            + 0.2 * (TAU * 719.0 * time).sin())
                        / 1.65
                })
                .collect()
        })
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    let sum: f64 = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum();
    (sum / samples.len() as f64).sqrt()
}

fn process_cpu_seconds() -> Result<f64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value points to a valid timespec for clock_gettime to initialize.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut value) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("clock_gettime");
    }
    Ok(value.tv_sec as f64 + value.tv_nsec as f64 / 1e9)
}

use std::f32::consts::TAU;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;
use voice_core::{VoiceEncoder, VoiceEncoderConfig};

const SAMPLE_RATE: usize = 48_000;
const FRAME_SAMPLES: usize = 960;
const FRAME_MS: usize = 20;
const SIGNAL_FRAMES: usize = 200;
const SILENCE: [f32; FRAME_SAMPLES] = [0.0; FRAME_SAMPLES];

fn main() -> Result<()> {
    let frames = std::env::args()
        .nth(1)
        .map(|value| value.parse())
        .transpose()
        .context("invalid frame count")?
        .unwrap_or(50_000);
    let signal = signal_frames();
    for workload in [Workload::Voiced, Workload::Silence, Workload::Mixed] {
        for dtx in [false, true] {
            measure(&signal, frames, workload, dtx)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Workload {
    Voiced,
    Silence,
    Mixed,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Voiced => "voiced",
            Self::Silence => "silence",
            Self::Mixed => "mixed_3s_voice_1s_silence",
        }
    }

    fn is_voiced(self, frame_index: usize) -> bool {
        match self {
            Self::Voiced => true,
            Self::Silence => false,
            Self::Mixed => frame_index % 200 < 150,
        }
    }
}

fn measure(signal: &[Vec<f32>], frames: usize, workload: Workload, dtx: bool) -> Result<()> {
    let mut encoder = VoiceEncoder::new(VoiceEncoderConfig {
        enable_dtx: dtx,
        ..VoiceEncoderConfig::default()
    })?;
    for i in 0..500 {
        encode_frame(&mut encoder, workload, i, signal)?;
    }

    let cpu_start = process_cpu_seconds()?;
    let wall_start = Instant::now();
    let mut packets = 0_u64;
    let mut payload_bytes = 0_u64;
    for i in 0..frames {
        if let Some(payload_len) = encode_frame(&mut encoder, workload, i, signal)? {
            packets += 1;
            payload_bytes += payload_len as u64;
        }
    }
    let wall_seconds = wall_start.elapsed().as_secs_f64();
    let cpu_seconds = process_cpu_seconds()? - cpu_start;
    let simulated_seconds = frames as f64 * FRAME_MS as f64 / 1_000.0;
    println!(
        "{}",
        json!({
            "benchmark": "voice_core_encode",
            "opus_version": audiopus::version(),
            "workload": workload.name(),
            "dtx": dtx,
            "frames": frames,
            "packets": packets,
            "payload_bytes": payload_bytes,
            "payload_bitrate_bps": payload_bytes as f64 * 8.0 / simulated_seconds,
            "cpu_seconds": cpu_seconds,
            "wall_seconds": wall_seconds,
            "cpu_ns_per_frame": cpu_seconds * 1e9 / frames as f64,
        })
    );
    Ok(())
}

fn encode_frame(
    encoder: &mut VoiceEncoder,
    workload: Workload,
    index: usize,
    signal: &[Vec<f32>],
) -> Result<Option<usize>> {
    let frame: &[f32] = if workload.is_voiced(index) {
        &signal[index % signal.len()]
    } else {
        &SILENCE
    };
    encoder.push_pcm(frame);
    Ok(encoder.poll_packet()?.map(|packet| packet.payload.len()))
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

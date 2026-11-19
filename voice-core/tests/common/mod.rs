//! Shared by the receiver oracles. Everything here is generic over `SimReceiver`, so the same
//! oracle can be pointed at another receiver (the seeded-fault proof runs it against the
//! pre-timeline NetEq receiver).
#![allow(dead_code)]

use voice_core::fixture_harness::*;
use voice_core::{PlayoutConfig, PlayoutReceiver, VoiceEncoderConfig, VoicePacket};

pub const START_US: u64 = 1_000_000;

pub fn new_rx() -> PlayoutReceiver {
    PlayoutReceiver::new().unwrap()
}

pub fn new_rx_with(cfg: PlayoutConfig) -> PlayoutReceiver {
    PlayoutReceiver::with_config(cfg).unwrap()
}

pub fn run_end_us(packets: &[VoicePacket]) -> u64 {
    let last = packets.last().map(|p| p.timestamp).unwrap_or(0);
    let first = packets.first().map(|p| p.timestamp).unwrap_or(0);
    START_US + (last.wrapping_sub(first) as u64) * 1000 / 48 + 1_000_000
}

/// Steady 30 ms network delay, 0 local delay, minus the packets `drop` selects.
pub fn lossy_events(
    packets: &[VoicePacket],
    drop: impl Fn(usize) -> bool,
) -> Vec<Scheduled> {
    schedule(packets, START_US, 0.0, |i, _| {
        if drop(i) {
            Fate::Drop
        } else {
            Fate::Deliver {
                net_delay_us: 30_000,
                local_us: 0,
            }
        }
    })
}

/// Output index (in samples) of the first pull whose block carries audio, per the trace.
pub fn active_range(run: &Run) -> (usize, usize) {
    let first = run
        .trace
        .iter()
        .position(|t| t.playout_timestamp.is_some())
        .unwrap_or(0);
    let last = run
        .trace
        .iter()
        .rposition(|t| t.playout_timestamp.is_some())
        .unwrap_or(0);
    (first * PULL_SAMPLES, (last + 1) * PULL_SAMPLES)
}

/// A few seconds of voiced signal, encoded with default (DTX on) settings plus an END marker.
pub fn voiced_packets(seconds: f32, cfg: VoiceEncoderConfig) -> Vec<VoicePacket> {
    encode_stream(&voiced_signal(seconds, 0.3), cfg).unwrap()
}

/// The audible part of `x` with the last slot trimmed: first to last non-zero sample minus one
/// slot. The lossless decode ends where the stream does, and that edge is not a "step inside
/// the speech" (it is the end-of-stream cut), so it must not set the continuity yardstick.
pub fn interior(x: &[f32]) -> &[f32] {
    let first = x.iter().position(|v| *v != 0.0).unwrap_or(0);
    let last = x.iter().rposition(|v| *v != 0.0).unwrap_or(0);
    let end = last.saturating_sub(SLOT_SAMPLES).max(first);
    &x[first..end]
}

/// Deterministic white noise of the given RMS level in dBFS.
pub fn noise(n: usize, dbfs: f32, seed: u32) -> Vec<f32> {
    let amp = 10f32.powf(dbfs / 20.0) * 3f32.sqrt(); // uniform [-a, a] has rms a / sqrt(3)
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            amp * ((x >> 8) as f32 / (1u32 << 23) as f32 - 1.0)
        })
        .collect()
}

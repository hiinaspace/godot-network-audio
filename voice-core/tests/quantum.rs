//! Pull cadence. PipeWire quantum 1024 makes the output callback ask for 1024 samples every
//! 21.33 ms instead of 480 every 10 ms; the receiver's `pull` takes any length, and these
//! check that the audio, the concealment accounting and the playout timestamp do not depend
//! on the cadence.

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::{PlayoutConfig, VoiceEncoderConfig};

struct Out {
    audio: Vec<f32>,
    stats: voice_core::PlayoutStats,
    /// Every playout timestamp seen, one per pull.
    stamps: Vec<Option<u32>>,
}

fn run_cadence(pull: usize, drop: impl Fn(usize) -> bool, cfg: PlayoutConfig) -> Out {
    let packets = voiced_packets(4.0, VoiceEncoderConfig::default());
    let events = lossy_events(&packets, |i| !packets[i].payload.is_empty() && drop(i));
    let mut rx = new_rx_with(cfg);
    let period_us = (pull as u64 * 1_000_000 + 24_000) / 48_000;
    let mut rc = RunConfig::until(run_end_us(&packets));
    rc.pull_samples = pull;
    rc.pull_period_us = period_us;
    let run = run(&mut rx, &events, &rc);
    let stamps = run.trace.iter().map(|t| t.playout_timestamp).collect::<Vec<_>>();
    // Audio only: from the first non-zero sample to the last.
    let a = run.output.iter().position(|v| *v != 0.0).unwrap_or(0);
    let b = run.output.iter().rposition(|v| *v != 0.0).map(|i| i + 1).unwrap_or(0);
    Out {
        audio: run.output[a..b].to_vec(),
        stats: rx.stats(),
        stamps,
    }
}

fn no_stretch() -> PlayoutConfig {
    PlayoutConfig {
        time_stretch: false,
        ..PlayoutConfig::default()
    }
}

#[test]
fn quantum_1024_decodes_the_same_audio_as_480_lossless() {
    let a = run_cadence(480, |_| false, no_stretch());
    let b = run_cadence(1024, |_| false, no_stretch());
    assert_eq!(b.stats.plc_samples, 0);
    assert_eq!(b.stats.late, 0);
    let n = a.audio.len().min(b.audio.len());
    assert!(n > 150_000, "{n} samples of audio");
    let worst = a.audio[..n]
        .iter()
        .zip(&b.audio[..n])
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-6, "audio differs between pull cadences by {worst}");
}

#[test]
fn quantum_1024_conceals_exactly_the_scripted_losses_and_stays_continuous() {
    for (every, pct) in [(50usize, 2u64), (20, 5)] {
        let clean = run_cadence(1024, |_| false, PlayoutConfig::default());
        let lossy = run_cadence(1024, |i| i % every == every - 1, PlayoutConfig::default());
        let s = &lossy.stats;
        let slots = clean.stats.packets_decoded;
        let lost = slots / every as u64;
        assert_eq!(s.plc_samples, lost * 960, "loss {pct} %: plc samples");
        assert_eq!(s.late, 0);
        let step = max_step(&lossy.audio);
        let clean_step = max_step(interior(&clean.audio));
        assert!(step <= clean_step * 1.5, "{pct} %: step {step:.4} vs {clean_step:.4}");
    }
}

#[test]
fn quantum_1024_playout_timestamp_advances_by_the_samples_pulled() {
    let o = run_cadence(1024, |_| false, no_stretch());
    let live: Vec<u32> = o.stamps.iter().flatten().copied().collect();
    assert!(live.len() > 100);
    let mut within = 0;
    for w in live.windows(2) {
        let d = w[1].wrapping_sub(w[0]);
        // consecutive live pulls advance by exactly one pull
        assert!(d == 1024 || d > 1024, "timestamp went {d} samples between two pulls");
        if d == 1024 {
            within += 1;
        }
    }
    assert!(within as usize > live.len() * 9 / 10, "{within} of {} steps were exactly 1024", live.len());
}

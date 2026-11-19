//! Level control on a steady tone: a stretch (Accelerate / Preemptive Expand) must not put a
//! click into the audio. A pure tone shows a bad splice as a sample-to-sample step far above
//! the tone's own maximum slope.

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::{PlayoutConfig, VoiceEncoderConfig};

fn tone(seconds: f32, hz: f64, amp: f32) -> Vec<f32> {
    (0..(seconds * 48_000.0) as usize)
        .map(|i| amp * (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin() as f32)
        .collect()
}

struct Out {
    max_step: f32,
    at: usize,
    ops: (u64, u64),
    plc: u64,
}

fn run_tone(ppm: f64, jitter_ms: u64, seed: u64, hz: f64) -> Out {
    let packets = encode_stream(&tone(20.0, hz, 0.3), VoiceEncoderConfig::default()).unwrap();
    let mut rng = seed;
    let events = schedule(&packets, START_US, ppm, |_, p| {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = if p.payload.is_empty() { 0 } else { (rng >> 33) % (jitter_ms * 1000 + 1) };
        Fate::Deliver { net_delay_us: 30_000 + j, local_us: 0 }
    });
    let mut rx = new_rx_with(PlayoutConfig::default());
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    // From one second after the first audio to one second before the last.
    let first = run.output.iter().position(|v| *v != 0.0).unwrap();
    let last = run.output.iter().rposition(|v| *v != 0.0).unwrap();
    let (a, b) = (first + 48_000, last - 48_000);
    let seg = &run.output[a..b];
    let (mut m, mut at) = (0.0f32, 0);
    for (i, w) in seg.windows(2).enumerate() {
        let d = (w[1] - w[0]).abs();
        if d > m {
            m = d;
            at = a + i;
        }
    }
    let s = rx.stats();
    Out { max_step: m, at, ops: (s.accelerate_ops, s.preemptive_ops), plc: s.plc_samples }
}

#[test]
fn stretching_a_steady_tone_leaves_no_click() {
    let hz = 440.0;
    let slope = 0.3 * std::f64::consts::TAU * hz / 48_000.0;
    let mut worst = (0.0f32, String::new());
    let mut total_ops = (0, 0);
    for (ppm, jitter, seed) in [(0.0, 0, 1), (0.0, 12, 2), (-500.0, 4, 3), (500.0, 4, 4), (-2000.0, 8, 5), (2000.0, 8, 6), (0.0, 25, 7), (-1000.0, 15, 8)] {
        let o = run_tone(ppm, jitter, seed, hz);
        eprintln!("ppm {ppm:+} jitter {jitter} ms: max step {:.4} ({:.1}x the tone's slope) at {:.3} s, accelerate/preemptive ops {:?}, plc {}", o.max_step, o.max_step as f64 / slope, o.at as f64 / 48_000.0, o.ops, o.plc);
        total_ops.0 += o.ops.0;
        total_ops.1 += o.ops.1;
        if o.max_step > worst.0 {
            worst = (o.max_step, format!("ppm {ppm} jitter {jitter} seed {seed} at {:.3} s", o.at as f64 / 48_000.0));
        }
    }
    eprintln!("total ops {total_ops:?}");
    assert!(total_ops.0 + total_ops.1 > 0, "the scenarios never stretch");
    assert!(
        (worst.0 as f64) < 2.0 * slope,
        "stretching put a step of {:.4} ({:.1}x the tone's slope) into a steady tone: {}",
        worst.0,
        worst.0 as f64 / slope,
        worst.1
    );
}

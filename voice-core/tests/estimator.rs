//! The delay estimator and the buffer it steers, on a simulated clock.

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::VoiceEncoderConfig;

fn nodtx() -> VoiceEncoderConfig {
    VoiceEncoderConfig { enable_dtx: false, ..VoiceEncoderConfig::default() }
}

fn target_at(run: &Run, ms: u64) -> f64 {
    run.trace[((ms + START_US / 1000) / 10) as usize].target_ms
}

#[test]
fn a_step_in_network_delay_raises_the_target_and_stays_there() {
    let packets = encode_stream(&voiced_signal(12.0, 0.3), nodtx()).unwrap();
    let events = schedule(&packets, START_US, 0.0, |i, _| Fate::Deliver {
        net_delay_us: if i < 250 { 30_000 } else { 130_000 },
        local_us: 0,
    });
    let mut rx = new_rx();
    let r = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let before = target_at(&r, 4_000);
    let after = target_at(&r, 5_500);
    assert!(before < 50.0, "target before the step: {before}");
    assert!(after >= 100.0 && after <= 250.0, "target after the step: {after}");
    let s = rx.stats();
    // The step itself costs a few late packets, then it settles.
    assert!(s.late <= 6, "late {}", s.late);
    assert!(s.plc_samples <= 6 * 960, "plc {}", s.plc_samples);
}

#[test]
fn a_single_sparse_spike_raises_the_target_at_once() {
    let packets = encode_stream(&voiced_signal(10.0, 0.3), nodtx()).unwrap();
    let events = schedule(&packets, START_US, 0.0, |i, _| Fate::Deliver {
        net_delay_us: if i == 200 { 110_000 } else { 30_000 },
        local_us: 0,
    });
    let mut rx = new_rx();
    let r = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    // Packet 200 arrives (nominally) at 4.0 s after the first; its spike is +80 ms.
    let before = target_at(&r, 3_900);
    let just_after = target_at(&r, 4_200);
    assert!(before < 50.0, "before {before}");
    assert!(just_after >= 85.0, "after the spike {just_after}");
    // And the slow decay brings it back down eventually.
    assert!(target_at(&r, 9_500) < just_after, "no decay");
}

#[test]
fn a_late_local_reader_is_counted_apart_from_the_network() {
    // The daemon saw every packet on time; the reader got a few 150 ms late.
    let packets = encode_stream(&voiced_signal(6.0, 0.3), nodtx()).unwrap();
    let events = schedule(&packets, START_US, 0.0, |i, _| Fate::Deliver {
        net_delay_us: 30_000,
        local_us: if (100..104).contains(&i) { 150_000 } else { 0 },
    });
    let mut rx = new_rx();
    run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let s = rx.stats();
    assert!(s.late >= 1);
    assert_eq!(s.local_late, s.late, "all of these misses are local");
    assert!(s.local_delay_max_ms >= 149.0);
    assert!(s.target_delay_ms < 60.0, "the network estimator was polluted: {}", s.target_delay_ms);
}

fn drift(ppm: f64) {
    let packets = encode_stream(&voiced_signal(60.0, 0.3), nodtx()).unwrap();
    let mut x = 12345u32;
    let events = schedule(&packets, START_US, ppm, |_, _| {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        Fate::Deliver { net_delay_us: 30_000 + ((x >> 16) % 6_000) as u64, local_us: 0 }
    });
    let mut rx = new_rx();
    let r = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let s = rx.stats();
    let active: Vec<_> = r.trace.iter().filter(|t| t.playout_timestamp.is_some()).collect();
    let (lo, hi) = active
        .iter()
        .fold((f64::MAX, 0.0f64), |(l, h), t| (l.min(t.buffer_ms), h.max(t.buffer_ms)));
    println!("drift {ppm:+} ppm: buffer {lo:.0}..{hi:.0} ms, plc {}, late {}, accel {}, pee {}", s.plc_samples, s.late, s.accelerate_samples, s.preemptive_samples);
    assert!(hi <= 250.0, "buffer grew to {hi} ms");
    assert_eq!(s.plc_samples, 0, "underruns");
    assert_eq!(s.late, 0, "late packets");
    assert_eq!(s.talkspurts, 1);
    assert!(s.target_delay_ms <= 100.0);
}

#[test]
fn sender_clock_500ppm_slow_without_dtx_stays_bounded_and_underrun_free() {
    drift(500.0);
}

#[test]
fn sender_clock_500ppm_fast_without_dtx_stays_bounded_and_underrun_free() {
    drift(-500.0);
}

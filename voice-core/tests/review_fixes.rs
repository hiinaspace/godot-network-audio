//! Regressions for the senior review of the timeline receiver (see docs/room/VOICE.md).

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::{PacketObservation, PushOutcome, VoiceEncoderConfig};

fn no_dtx() -> VoiceEncoderConfig {
    VoiceEncoderConfig { enable_dtx: false, ..VoiceEncoderConfig::default() }
}

fn run_late(late_ms: u64, every: usize) -> (voice_core::PlayoutStats, Vec<f32>) {
    let packets = encode_stream(&voiced_signal(10.0, 0.3), no_dtx()).unwrap();
    let events = schedule(&packets, START_US, 0.0, |i, _| Fate::Deliver {
        net_delay_us: 30_000 + if i % every == 12 { late_ms * 1000 } else { 0 },
        local_us: 0,
    });
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    (rx.stats(), run.output)
}

fn clean_step() -> f32 {
    let packets = encode_stream(&voiced_signal(10.0, 0.3), no_dtx()).unwrap();
    let events = lossy_events(&packets, |_| false);
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    max_step(interior(&run.output))
}

/// Finding 1+2: one packet in 25 arriving 0.8 s late is far beyond what the buffer can cover.
/// It must neither pin the target at the cap nor make the receiver skip audio.
#[test]
fn a_packet_0_8s_late_every_25th_causes_no_skip_and_no_splice() {
    let clean = clean_step();
    let (s, out) = run_late(800, 25);
    assert!(s.late >= 6, "late {}", s.late);
    assert_eq!(s.skipped_samples, 0, "skipped samples");
    assert!(s.target_delay_ms <= 60.0, "target {} ms", s.target_delay_ms);
    let step = max_step(interior(&out));
    println!("late run: skipped {} target {:.0} ms step {step:.4} vs lossless {clean:.4}", s.skipped_samples, s.target_delay_ms);
    assert!(step <= clean * 1.5, "step {step:.4} vs lossless {clean:.4}");
}

/// A packet 0.3 s late is inside what the buffer can hold: the target rises, the buffer builds
/// by Preemptive Expand, and nothing is skipped (a hard splice) at the top of the range.
#[test]
fn a_packet_0_3s_late_every_25th_builds_the_buffer_without_a_skip() {
    let clean = clean_step();
    let (s, out) = run_late(300, 25);
    assert_eq!(s.skipped_samples, 0, "skipped samples");
    let step = max_step(interior(&out));
    println!("late run: skipped {} target {:.0} ms step {step:.4} vs lossless {clean:.4}", s.skipped_samples, s.target_delay_ms);
    assert!(step <= clean * 1.5, "step {step:.4} vs lossless {clean:.4}");
}

/// Finding 1: a duplicate of a packet far older than the remembered ones must not feed the
/// estimator its (huge) apparent delay.
#[test]
fn a_very_old_duplicate_does_not_feed_the_estimator() {
    let packets = encode_stream(&voiced_signal(6.0, 0.3), no_dtx()).unwrap();
    let events = lossy_events(&packets, |_| false);
    let mut rx = new_rx();
    run(&mut rx, &events, &RunConfig::until(START_US + 4_000_000));
    let before = rx.stats().peak_delay_ms;
    let t = START_US + 4_100_000;
    let out = rx.push_packet(packets[3].clone(), PacketObservation::same(t));
    assert!(matches!(out, PushOutcome::Late | PushOutcome::Duplicate), "{out:?}");
    assert_eq!(rx.stats().peak_delay_ms, before, "peak moved");
}

/// Finding 3: the continuity yardstick must not contain the end-of-stream cut.
#[test]
fn the_yardstick_excludes_the_end_cut() {
    let packets = encode_stream(&voiced_signal(4.0, 0.3), no_dtx()).unwrap();
    let events = lossy_events(&packets, |_| false);
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let inner = max_step(interior(&run.output));
    let (a, b) = active_range(&run);
    let whole = max_step(&run.output[a..b]);
    assert!(inner <= whole);
    println!("interior {inner:.4} whole {whole:.4}");
}

/// Finding 5: where audio ends (END marker) the output fades over one concealed frame instead
/// of stepping to zero.
#[test]
fn a_talkspurt_end_has_no_hard_cut() {
    let packets = encode_stream(&voiced_signal(3.0, 0.3), VoiceEncoderConfig::default()).unwrap();
    let events = lossy_events(&packets, |_| false);
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let inner = max_step(interior(&run.output));
    let (_, b) = active_range(&run);
    let tail = max_step(&run.output[b.saturating_sub(4 * 960)..run.output.len()]);
    println!("end step {tail:.4} vs interior {inner:.4}");
    assert!(tail <= inner * 1.5, "end step {tail:.4} vs interior {inner:.4}");
    assert!(rx.stats().fade_samples > 0);
}

/// Finding 5: a loss run past the PLC cap fades out at the cap.
#[test]
fn a_loss_past_the_plc_cap_fades_out_without_a_step() {
    let packets = encode_stream(&voiced_signal(4.0, 0.3), no_dtx()).unwrap();
    let events = lossy_events(&packets, |i| (60..85).contains(&i));
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let inner = max_step(interior(&run.output));
    // The loss starts at slot 60; look at output from 1.0 s + 60 slots to the resume.
    let from = ((1.0 + 60.0 * 0.02) * 48_000.0) as usize;
    let to = ((1.0 + 85.0 * 0.02) * 48_000.0) as usize;
    let seg = &run.output[from..to];
    let s = max_step(seg);
    assert!(s <= inner * 1.5, "loss-window step {s:.4} vs interior {inner:.4}");
    // For this signal Opus's own PLC has decayed by the cap; the fade is what guarantees it.
    assert!(rx.stats().fade_samples >= 960, "no fade at the cap");
}

/// Finding 7: markers alone in an idle receiver do not count as waiting for an onset.
#[test]
fn markers_do_not_count_as_onset_wait() {
    use voice_core::VoicePacket;
    let packets = encode_stream(&voiced_signal(1.0, 0.3), no_dtx()).unwrap();
    let mut rx = new_rx();
    // A stray END marker, then a second of idle pulls.
    let marker = VoicePacket {
        seq: 900,
        timestamp: 5_000_000,
        flags: voice_core::PacketFlags::from_bits(voice_core::PacketFlags::END_OF_TALKSPURT),
        payload: Vec::new(),
    };
    rx.push_packet(marker, PacketObservation::same(0));
    let mut buf = vec![0.0f32; 480];
    for _ in 0..100 {
        rx.pull(&mut buf);
    }
    // First packet of a talkspurt. With the default target the receiver has to wait for more.
    let mut p0 = packets[0].clone();
    p0.timestamp = 5_000_000 + 960;
    rx.push_packet(p0, PacketObservation::same(1_000_000));
    rx.pull(&mut buf);
    assert!(rx.stats().idle, "started on one packet because a marker was 'waiting'");
}

/// Finding 2: when the path delay drops by half a second the buffer jumps by 25 slots, far more
/// than Accelerate can take down. The receiver skips ahead, and the skip is a cross-fade, not a
/// splice. A hard splice lands at an arbitrary phase, so several tones are tried: one lucky
/// phase proves nothing.
#[test]
fn a_skip_after_a_delay_drop_is_a_crossfade_not_a_splice() {
    let mut worst = 0.0f32;
    for hz in [613.0, 713.0, 811.0, 947.0, 1103.0] {
        let tone = chirp(10.0, hz, hz, 0.3);
        let packets = encode_stream(&tone, no_dtx()).unwrap();
        let clean = {
            let events = lossy_events(&packets, |_| false);
            let mut rx = new_rx();
            let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
            max_step(interior(&run.output))
        };
        let events = schedule(&packets, START_US, 0.0, |i, _| Fate::Deliver {
            net_delay_us: if i < 250 { 530_000 } else { 30_000 },
            local_us: 0,
        });
        let mut rx = new_rx();
        let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
        let s = rx.stats();
        let step = max_step(interior(&run.output));
        println!("skip {hz} Hz: skipped {} samples, step {step:.4} vs lossless {clean:.4} (x{:.2})", s.skipped_samples, step / clean);
        assert!(s.skipped_samples > 0, "no skip happened: the scenario does not test it");
        worst = worst.max(step / clean);
    }
    assert!(worst <= 1.5, "worst step is x{worst:.2} the tone's own");
}

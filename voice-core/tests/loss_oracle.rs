//! The loss oracle: a real Opus encode of a voiced signal, scripted packet drops, a simulated
//! clock. These fail on the pre-timeline receiver (docs/room/VOICE.md records the numbers).

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::{PlayoutConfig, VoiceEncoderConfig};

struct Outcome {
    /// The audio part of the output (from the first block with a playout timestamp).
    active: Vec<f32>,
    slots: usize,
    stats: voice_core::PlayoutStats,
}

fn run_with_drops(cfg: PlayoutConfig, enc: VoiceEncoderConfig, drop: impl Fn(usize) -> bool) -> Outcome {
    let packets = voiced_packets(4.0, enc);
    // Voiced packets only (the END marker is the last packet).
    let slots = packets.iter().filter(|p| !p.payload.is_empty()).count();
    let events = lossy_events(&packets, |i| !packets[i].payload.is_empty() && drop(i));
    let mut rx = new_rx_with(cfg);
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let (a, b) = active_range(&run);
    Outcome {
        active: run.output[a..b].to_vec(),
        slots,
        stats: rx.stats(),
    }
}


/// Max step of the lossless decode, as the continuity yardstick. Measured on the interior
/// (first to last non-zero sample minus one slot): the lossless run ends with the stream, and
/// that edge is not a step inside the speech.
fn clean_max_step() -> (f32, Outcome) {
    let clean = run_with_drops(PlayoutConfig::default(), VoiceEncoderConfig::default(), |_| false);
    (max_step(interior(&clean.active)), clean)
}

#[test]
fn no_loss_plays_every_slot_with_no_concealment() {
    let (_, clean) = clean_max_step();
    assert_eq!(clean.stats.plc_samples, 0);
    assert_eq!(clean.stats.late, 0);
    assert_eq!(clean.stats.output_samples, clean.slots as u64 * 960);
    assert_eq!(clean.stats.accelerate_samples + clean.stats.preemptive_samples, 0);
}

fn check_loss(drop: impl Fn(usize) -> bool + Copy, dropped_expected: usize, label: &str) {
    let (clean_step, _clean) = clean_max_step();
    let lossy = run_with_drops(PlayoutConfig::default(), VoiceEncoderConfig::default(), drop);
    let s = &lossy.stats;

    // (a) No slot vanishes: the same number of samples comes out as for the lossless run.
    // Level control may stretch or squeeze time; that is accounted for, and nothing else.
    assert_eq!(
        s.output_samples + s.accelerate_samples,
        lossy.slots as u64 * 960 + s.preemptive_samples,
        "{label}: output samples {} for {} slots",
        s.output_samples,
        lossy.slots
    );
    // With level control off the count is exact.
    let exact = run_with_drops(
        PlayoutConfig { time_stretch: false, ..PlayoutConfig::default() },
        VoiceEncoderConfig::default(),
        drop,
    );
    assert_eq!(exact.stats.output_samples, lossy.slots as u64 * 960, "{label}: exact count");
    assert!(max_step(&exact.active) <= clean_step * 1.5, "{label}: unstretched continuity");
    assert_eq!(s.plc_samples, dropped_expected as u64 * 960, "{label}: plc samples");

    // (b) The waveform stays continuous through concealment: no step beyond what the lossless
    // decode itself contains (with 50 % headroom).
    let step = max_step(&lossy.active);
    assert!(
        step <= clean_step * 1.5,
        "{label}: max step {step:.4} vs lossless {clean_step:.4}"
    );

    // (c) Measured concealment matches the scripted loss rate.
    let want = 100.0 * dropped_expected as f64 / lossy.slots as f64;
    assert!(
        (exact.stats.concealed_pct() - want).abs() < 0.01 && (s.concealed_pct() - want).abs() < 0.2,
        "{label}: concealed {:.3} % vs scripted {:.3} %",
        s.concealed_pct(),
        want
    );
    println!("{label}: concealed {:.2} %, max step {step:.4} (lossless {clean_step:.4})", s.concealed_pct());
}

#[test]
fn every_50th_packet_lost_is_2_percent_concealed_and_continuous() {
    let n = voiced_packets(4.0, VoiceEncoderConfig::default())
        .iter()
        .filter(|p| !p.payload.is_empty())
        .count();
    let dropped = (0..n).filter(|i| i % 50 == 25).count();
    check_loss(|i| i % 50 == 25, dropped, "every 50th");
}

#[test]
fn every_20th_packet_lost_is_5_percent_concealed_and_continuous() {
    let n = voiced_packets(4.0, VoiceEncoderConfig::default())
        .iter()
        .filter(|p| !p.payload.is_empty())
        .count();
    let dropped = (0..n).filter(|i| i % 20 == 10).count();
    check_loss(|i| i % 20 == 10, dropped, "every 20th");
}

#[test]
fn bursts_of_two_and_three_are_concealed_and_continuous() {
    let is_burst = |i: usize| matches!(i % 60, 20 | 21) || matches!(i % 90, 55..=57);
    let n = voiced_packets(4.0, VoiceEncoderConfig::default())
        .iter()
        .filter(|p| !p.payload.is_empty())
        .count();
    let dropped = (0..n).filter(|&i| is_burst(i)).count();
    check_loss(is_burst, dropped, "bursts");
}

#[test]
fn fec_recovers_single_losses_when_the_sender_sends_it() {
    let enc = VoiceEncoderConfig {
        inband_fec: true,
        packet_loss_perc: 10,
        bitrate_bps: 24_000,
        ..VoiceEncoderConfig::default()
    };
    let cfg = PlayoutConfig {
        fec: true,
        ..PlayoutConfig::default()
    };
    // Which packets carry LBRR depends on the signal; a loss is recoverable exactly when the
    // following packet carries it.
    let packets = voiced_packets(4.0, enc.clone());
    let lost = |i: usize| i % 20 == 10 && !packets[i].payload.is_empty();
    let recoverable = (0..packets.len())
        .filter(|&i| lost(i) && voice_core::decoder::payload_has_fec(&packets[i + 1].payload))
        .count();
    let total = (0..packets.len()).filter(|&i| lost(i)).count();
    assert!(recoverable > 0 && recoverable < total + 1);
    let lossy = run_with_drops(cfg.clone(), enc.clone(), |i| i % 20 == 10);
    let s = &lossy.stats;
    assert_eq!(s.fec_samples, recoverable as u64 * 960, "FEC slots");
    assert_eq!(s.plc_samples, (total - recoverable) as u64 * 960, "PLC slots");
    assert_eq!(s.output_samples + s.accelerate_samples, lossy.slots as u64 * 960 + s.preemptive_samples);
    let clean = run_with_drops(cfg, enc, |_| false);
    assert!(
        max_step(&lossy.active) <= max_step(interior(&clean.active)) * 1.5,
        "FEC recovery is discontinuous"
    );
}

#[test]
fn rejected_packets_are_counted_and_never_played() {
    use voice_core::{PacketObservation, PushOutcome, VoicePacket};
    let mut rx = new_rx();
    let obs = PacketObservation::same(1_000);
    // 40 ms frame and garbage.
    let enc40 = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Mono,
        audiopus::Application::Voip,
    )
    .unwrap();
    let mut out = vec![0u8; 400];
    let n = enc40.encode_float(&vec![0.1f32; 1920], &mut out).unwrap();
    out.truncate(n);
    let long = VoicePacket {
        seq: 1,
        timestamp: 0,
        flags: Default::default(),
        payload: out,
    };
    assert_eq!(rx.push_packet(long, obs), PushOutcome::Rejected);
    let junk = VoicePacket {
        seq: 2,
        timestamp: 960,
        flags: Default::default(),
        payload: vec![0xff],
    };
    assert_eq!(rx.push_packet(junk, obs), PushOutcome::Rejected);
    // Stereo
    let encs = audiopus::coder::Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Voip,
    )
    .unwrap();
    let mut o2 = vec![0u8; 400];
    let n = encs.encode_float(&vec![0.1f32; 1920], &mut o2).unwrap();
    o2.truncate(n);
    let st = VoicePacket {
        seq: 3,
        timestamp: 1920,
        flags: Default::default(),
        payload: o2,
    };
    assert_eq!(rx.push_packet(st, obs), PushOutcome::Rejected);
    let s = rx.stats();
    assert_eq!(s.rejected, 3);
    assert_eq!(s.packets_in, 0);
    assert!(rx.is_idle());
}

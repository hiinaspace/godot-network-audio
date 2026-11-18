//! Reordered, duplicate and late packets: nothing plays after a newer timestamp, counters add up.

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::{PlayoutConfig, VoiceEncoderConfig};

fn cfg100() -> PlayoutConfig {
    PlayoutConfig {
        min_delay_ms: 100,
        time_stretch: false,
        ..PlayoutConfig::default()
    }
}

#[test]
fn reordered_duplicate_and_late_packets_never_play_out_of_order() {
    let enc = VoiceEncoderConfig {
        enable_dtx: false,
        ..VoiceEncoderConfig::default()
    };
    // 300 -> 3000 Hz over 3 s, so the dominant frequency says where in the timeline we are.
    let packets = encode_stream(&chirp(3.0, 300.0, 3000.0, 0.3), enc).unwrap();
    let voiced: Vec<_> = packets.iter().filter(|p| !p.payload.is_empty()).collect();
    let slots = voiced.len();
    let mut events = schedule(&packets, START_US, 0.0, |i, _| {
        let net = match i {
            // Two packets overtaken by their successors, still well ahead of the cursor.
            40 | 70 => 85_000,
            // One that arrives far too late.
            100 => 700_000,
            _ => 30_000,
        };
        Fate::Deliver { net_delay_us: net, local_us: 0 }
    });
    // A duplicate of packet 60, 20 ms after the original.
    let dup = events.iter().find(|e| e.packet.seq == packets[60].seq).unwrap().clone();
    events.push(Scheduled {
        daemon_us: dup.daemon_us + 20_000,
        local_us: dup.local_us + 20_000,
        ..dup
    });
    events.sort_by_key(|e| e.local_us);

    let mut rx = new_rx_with(cfg100());
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let s = rx.stats();

    assert_eq!(s.late, 1, "late");
    assert_eq!(s.duplicate, 1, "duplicate");
    assert!(s.reordered_accepted >= 2, "reordered_accepted {}", s.reordered_accepted);
    assert_eq!(s.output_samples, slots as u64 * 960, "no slot vanishes or repeats");
    assert_eq!(s.plc_samples, 960, "exactly the late packet's slot is concealed");
    assert_eq!(s.packets_decoded, slots as u64 - 1);

    // Playout timestamps never step backwards (wrap-aware).
    let ts: Vec<u32> = run.trace.iter().filter_map(|t| t.playout_timestamp).collect();
    assert!(ts.len() > 100);
    for w in ts.windows(2) {
        assert!((w[1].wrapping_sub(w[0]) as i32) >= 0, "timestamp went back: {} -> {}", w[0], w[1]);
    }

    // The chirp comes out rising: dominant frequency per 100 ms window never falls by more
    // than a quantisation step.
    let (a, b) = active_range(&run);
    let bank: Vec<f64> = (0..=140).map(|i| 200.0 + 25.0 * i as f64).collect();
    let freqs: Vec<f64> = run.output[a..b]
        .chunks_exact(4800)
        .map(|w| dominant_frequency(w, &bank))
        .collect();
    for w in freqs.windows(2) {
        assert!(w[1] + 75.0 >= w[0], "chirp fell back: {} -> {} Hz", w[0], w[1]);
    }
}

#[test]
fn a_packet_behind_a_newer_one_is_late_not_played() {
    use voice_core::{PacketObservation, PushOutcome};
    let enc = VoiceEncoderConfig {
        enable_dtx: false,
        ..VoiceEncoderConfig::default()
    };
    let packets = encode_stream(&chirp(1.0, 300.0, 1000.0, 0.3), enc).unwrap();
    let mut rx = new_rx_with(cfg100());
    let obs = |us| PacketObservation::same(us);
    assert_eq!(rx.push_packet(packets[5].clone(), obs(0)), PushOutcome::Queued);
    assert_eq!(rx.push_packet(packets[7].clone(), obs(20_000)), PushOutcome::Queued);
    assert_eq!(rx.push_packet(packets[6].clone(), obs(25_000)), PushOutcome::Reordered);
    assert_eq!(rx.push_packet(packets[6].clone(), obs(26_000)), PushOutcome::Duplicate);
    // Drain well past packet 6's slot, then offer one from before the cursor.
    let mut buf = vec![0.0; 480];
    for _ in 0..50 {
        rx.pull(&mut buf);
    }
    assert_eq!(rx.push_packet(packets[2].clone(), obs(600_000)), PushOutcome::Late);
    let s = rx.stats();
    assert_eq!((s.late, s.duplicate, s.reordered_accepted), (1, 1, 1));
}

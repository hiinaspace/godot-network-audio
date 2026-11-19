//! DTX refreshes, talkspurts, wrap-around.

mod common;

use common::*;
use voice_core::fixture_harness::*;
use voice_core::{PlayoutConfig, VoiceEncoderConfig, VoicePacket};

/// 1 s voiced, 4 s silence, 0.6 s voiced, 4 s silence.
fn spurty_signal() -> Vec<f32> {
    let mut s = voiced_signal(1.0, 0.3);
    s.extend(vec![0.0; 48_000 * 4]);
    s.extend(voiced_signal(0.6, 0.3));
    s.extend(vec![0.0; 48_000 * 4]);
    s
}

fn play(packets: &[VoicePacket]) -> (voice_core::PlayoutStats, Run) {
    let events = lossy_events(packets, |_| false);
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(packets)));
    (rx.stats(), run)
}

#[test]
fn dtx_refreshes_cause_no_target_reset_and_no_prebuffer() {
    let packets = encode_stream(&spurty_signal(), VoiceEncoderConfig::default()).unwrap();
    let (s, _) = play(&packets);
    // Two real talkspurts; the refresh packets in the silences are neither.
    assert_eq!(s.talkspurts, 2, "talkspurts");
    assert_eq!(s.prebuffers, 2, "prebuffers: one per real onset, none per refresh");
    assert_eq!(s.target_resets, 0, "target resets");
    assert!(s.dtx_refreshes >= 4, "dtx_refreshes {}", s.dtx_refreshes);
    assert_eq!(s.late, 0);
}

#[test]
fn silence_between_talkspurts_is_digital_zero_and_idle() {
    let packets = encode_stream(&spurty_signal(), VoiceEncoderConfig::default()).unwrap();
    let events = lossy_events(&packets, |_| false);
    let mut rx = new_rx();
    let mut idle_seen = false;
    let mut t = START_US;
    let mut next = 0;
    let mut buf = vec![0.0; 480];
    let mut quiet_blocks = 0;
    while t < run_end_us(&packets) {
        while next < events.len() && events[next].local_us <= t {
            let e = &events[next];
            rx.push_packet(e.packet.clone(), voice_core::PacketObservation { daemon_arrival_us: e.daemon_us, local_us: e.local_us });
            next += 1;
        }
        rx.pull(&mut buf);
        if rx.is_idle() {
            idle_seen = true;
            if rx.playout_timestamp().is_none() && buf.iter().all(|v| *v == 0.0) {
                quiet_blocks += 1;
            }
        }
        t += 10_000;
    }
    assert!(idle_seen);
    assert!(quiet_blocks > 300, "only {quiet_blocks} digital-silence blocks");
}

#[test]
fn seq_and_timestamp_wrap_mid_stream() {
    let enc = VoiceEncoderConfig { enable_dtx: false, ..VoiceEncoderConfig::default() };
    let mut packets = encode_stream(&voiced_signal(3.0, 0.3), enc).unwrap();
    let n = packets.iter().filter(|p| !p.payload.is_empty()).count();
    // Start 20 packets before both counters wrap.
    for (i, p) in packets.iter_mut().enumerate() {
        p.seq = 65_516u16.wrapping_add(i as u16);
        p.timestamp = u32::MAX.wrapping_sub(20 * 960 - 1).wrapping_add(i as u32 * 960);
    }
    // Plus a scripted loss and a reorder across the wrap.
    let events = schedule(&packets, START_US, 0.0, |i, _| {
        if i == 21 {
            Fate::Drop
        } else {
            Fate::Deliver { net_delay_us: if i == 19 { 60_000 } else { 30_000 }, local_us: 0 }
        }
    });
    let mut rx = new_rx_with(PlayoutConfig { min_delay_ms: 100, time_stretch: false, ..PlayoutConfig::default() });
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let s = rx.stats();
    assert_eq!(s.output_samples, n as u64 * 960);
    assert_eq!(s.plc_samples, 960);
    assert_eq!(s.late, 0);
    assert_eq!(s.duplicate, 0);
    assert!(s.reordered_accepted >= 1);
    assert_eq!(s.talkspurts, 1);
    let ts: Vec<u32> = run.trace.iter().filter_map(|t| t.playout_timestamp).collect();
    assert!(ts.iter().any(|t| *t < 1_000_000) && ts.iter().any(|t| *t > u32::MAX - 100_000), "no wrap seen");
    for w in ts.windows(2) {
        assert!((w[1].wrapping_sub(w[0]) as i32) >= 0);
    }
}

#[test]
fn consecutive_loss_beyond_the_cap_goes_idle_then_resumes() {
    let enc = VoiceEncoderConfig { enable_dtx: false, ..VoiceEncoderConfig::default() };
    let packets = encode_stream(&voiced_signal(4.0, 0.3), enc).unwrap();
    // Lose half a second (25 slots) mid-stream.
    let events = lossy_events(&packets, |i| (60..85).contains(&i));
    let mut rx = new_rx();
    run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let s = rx.stats();
    assert!(s.plc_samples <= 7 * 960, "PLC ran {} samples", s.plc_samples);
    assert!(s.talkspurts >= 2, "talkspurts {}", s.talkspurts);
    // 25 slots lost: the cap is 6 concealed frames, the last of them the fade-out; the other 19
    // are time that is gone, and the stats must say so (they used to read as nothing at all).
    assert_eq!(s.plc_samples, 5 * 960, "plc");
    assert_eq!(s.fade_samples, 960 + 960, "fade: one at the cap, one at the end of the stream");
    assert_eq!(s.lost_unconcealed_samples, 19 * 960, "lost, unconcealed");
    assert_eq!(s.silence_samples, 0, "silence_samples is the sender's pauses, not losses");
}

/// -55 dBFS room noise in the pauses: Opus goes to DTX there and sends one refresh packet per
/// 400 ms. Each is a lone 20 ms packet; played, it would be a 2.5 Hz tick on digital silence.
#[test]
fn dtx_refreshes_in_a_noise_floor_are_not_played_as_blips() {
    let mut sig = voiced_signal(1.0, 0.3);
    sig.extend(noise(48_000 * 4, -55.0, 1));
    sig.extend(voiced_signal(0.6, 0.3));
    sig.extend(noise(48_000 * 4, -55.0, 2));
    let packets = encode_stream(&sig, VoiceEncoderConfig::default()).unwrap();
    let events = lossy_events(&packets, |_| false);
    let mut rx = new_rx();
    let run = run(&mut rx, &events, &RunConfig::until(run_end_us(&packets)));
    let s = rx.stats();
    assert!(s.dtx_refreshes >= 6, "only {} refreshes: the signal never entered DTX", s.dtx_refreshes);
    assert_eq!(s.talkspurts, 2);
    // Opus keeps coding the noise as speech for ~0.8 s (its DTX hangover) and then sends one
    // refresh per 400 ms: refreshes are at input 2.14, 2.56, ... 4.66 s and 6.64 s on. Output lags
    // the input by the 1 s start plus ~0.1 s. Look from just after the hangover's fade to just
    // before the next onset (input 5.0 s), and in the second pause.
    for (from, to) in [(2.95, 5.9), (7.45, 10.4)] {
        let pause = &run.output[(from * 48_000.0) as usize..(to * 48_000.0) as usize];
        let nonzero = pause.iter().filter(|v| **v != 0.0).count();
        assert_eq!(nonzero, 0, "{nonzero} non-zero samples in {from}..{to} s: refreshes were played");
    }
    // And the receiver kept decoding them (decoder state), it only did not play them.
    assert!(s.packets_decoded as usize >= packets.iter().filter(|p| !p.payload.is_empty()).count() - 2);
}

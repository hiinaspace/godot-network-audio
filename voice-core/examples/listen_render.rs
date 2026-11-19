//! Offline renderer for the blind listening packets (tools/listen). No audio playback, no real
//! time: everything runs on a simulated clock and writes raw f32le mono 48 kHz.
//!
//! cargo run --release --example listen_render -- pkts   IN.f32 OUT.events KBPS LOSS_PCT SEED
//! cargo run --release --example listen_render -- play   IN.events OUT.f32 OUT.stats.json
//! cargo run --release --example listen_render -- codec  IN.f32 OUT.f32 KBPS OUT.stats.json
//!
//! `pkts` encodes with the production sender settings (VOIP, 20 ms, DTX on, `KBPS`) and drops
//! packets from a seeded, deterministic pattern. The events file is what BOTH receivers are fed
//! (the legacy runner reads the same file), one line per delivered packet:
//! `daemon_us local_us seq timestamp flags payload_hex`.
use std::fmt::Write as _;
use std::fs;

use anyhow::{bail, Context, Result};
use serde_json::json;
use voice_core::fixture_harness::{encode_stream, run, schedule, Fate, RunConfig, SLOT_SAMPLES};
use voice_core::{PacketFlags, PlayoutReceiver, VoiceEncoderConfig, VoicePacket};

const START_US: u64 = 1_000_000;
const TAIL_US: u64 = 1_500_000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn read_f32(path: &str) -> Result<Vec<f32>> {
    let b = fs::read(path).with_context(|| path.to_string())?;
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn write_f32(path: &str, x: &[f32]) -> Result<()> {
    let mut b = Vec::with_capacity(x.len() * 4);
    for v in x {
        b.extend_from_slice(&v.to_le_bytes());
    }
    fs::write(path, b)?;
    Ok(())
}

/// Opus bandwidth from a packet's TOC byte (RFC 6716 section 3.1).
fn toc_bandwidth(toc: u8) -> &'static str {
    match toc >> 3 {
        0..=3 | 16..=19 => "NB",
        4..=7 => "MB",
        8..=11 | 20..=23 => "WB",
        12 | 13 | 24..=27 => "SWB",
        _ => "FB",
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for v in b {
        write!(s, "{v:02x}").unwrap();
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Choose the dropped packet indices. Exactly round(pct% of N) packets, never the first 5 or the
/// last 3 packets, never adjacent except in deliberate 2-packet bursts. At 5 % and above, two
/// bursts of 2 are placed first (these count towards the total).
fn loss_pattern(n: usize, pct: f64, seed: u64) -> Vec<usize> {
    let target = ((n as f64) * pct / 100.0).round() as usize;
    let mut rng = Rng(seed);
    let (lo, hi) = (5usize, n.saturating_sub(3));
    let mut dropped: Vec<usize> = Vec::new();
    let free = |dropped: &[usize], i: usize, w: usize| {
        // keep >= 3 delivered packets between separate loss events
        (i.saturating_sub(3)..i + w + 3).all(|j| !dropped.contains(&j))
    };
    if pct >= 5.0 {
        for _ in 0..2 {
            for _try in 0..10_000 {
                let i = lo + (rng.next() as usize) % (hi - lo - 1);
                if free(&dropped, i, 2) {
                    dropped.push(i);
                    dropped.push(i + 1);
                    break;
                }
            }
        }
    }
    let mut guard = 0;
    while dropped.len() < target && guard < 1_000_000 {
        guard += 1;
        let i = lo + (rng.next() as usize) % (hi - lo);
        if free(&dropped, i, 1) {
            dropped.push(i);
        }
    }
    dropped.sort_unstable();
    dropped
}

fn cmd_pkts(a: &[String]) -> Result<()> {
    let input = read_f32(&a[0])?;
    let kbps: i32 = a[2].parse()?;
    let pct: f64 = a[3].parse()?;
    let seed: u64 = a[4].parse()?;
    let cfg = VoiceEncoderConfig {
        bitrate_bps: kbps * 1000,
        ..VoiceEncoderConfig::default()
    };
    let packets = encode_stream(&input, cfg)?;
    let dropped = loss_pattern(packets.len(), pct, seed);
    let sched = schedule(&packets, START_US, 0.0, |i, _| {
        if dropped.contains(&i) {
            Fate::Drop
        } else {
            Fate::Deliver { net_delay_us: 0, local_us: 0 }
        }
    });
    let mut s = String::new();
    for e in &sched {
        writeln!(
            s,
            "{} {} {} {} {} {}",
            e.daemon_us,
            e.local_us,
            e.packet.seq,
            e.packet.timestamp,
            e.packet.flags.bits(),
            hex(&e.packet.payload)
        )?;
    }
    fs::write(&a[1], s)?;
    let voiced = packets.iter().filter(|p| !p.payload.is_empty()).count();
    let bursts = dropped.windows(2).filter(|w| w[1] == w[0] + 1).count();
    let meta = json!({
        "kbps": kbps, "loss_pct_target": pct, "seed": seed,
        "packets_total": packets.len(), "packets_voiced": voiced,
        "dropped_count": dropped.len(),
        "dropped_pct": 100.0 * dropped.len() as f64 / packets.len() as f64,
        "dropped_indices": dropped, "dropped_seq": dropped.iter().map(|&i| packets[i].seq).collect::<Vec<_>>(),
        "two_packet_bursts": bursts,
        "dropped_ms_in_stream": dropped.iter().map(|&i| packets[i].timestamp / 48).collect::<Vec<_>>(),
        "input_seconds": input.len() as f64 / 48_000.0,
    });
    fs::write(format!("{}.meta.json", a[1]), serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

fn read_events(path: &str) -> Result<Vec<(u64, u64, VoicePacket)>> {
    let mut out = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        out.push((
            f[0].parse()?,
            f[1].parse()?,
            VoicePacket {
                seq: f[2].parse()?,
                timestamp: f[3].parse()?,
                flags: PacketFlags::from_bits(f[4].parse()?),
                payload: unhex(f.get(5).copied().unwrap_or("")),
            },
        ));
    }
    Ok(out)
}

fn cmd_play(a: &[String]) -> Result<()> {
    let ev = read_events(&a[0])?;
    let sched: Vec<_> = ev
        .into_iter()
        .map(|(d, l, p)| voice_core::fixture_harness::Scheduled { packet: p, daemon_us: d, local_us: l })
        .collect();
    let end = sched.last().map(|e| e.local_us).unwrap_or(0) + TAIL_US;
    let mut rx = PlayoutReceiver::new()?;
    let mut cfg = RunConfig::until(end);
    cfg.start_us = START_US;
    let r = run(&mut rx, &sched, &cfg);
    write_f32(&a[1], &r.output)?;
    let s = rx.stats();
    fs::write(
        &a[2],
        serde_json::to_string_pretty(&json!({
            "receiver": "new PlayoutReceiver", "opus": voice_core::opus_version(),
            "output_samples": s.output_samples, "plc_samples": s.plc_samples, "fec_samples": s.fec_samples,
            "concealed_pct": s.concealed_pct(),
            "silence_samples": s.silence_samples, "packets_in": s.packets_in, "packets_decoded": s.packets_decoded,
            "late": s.late, "accelerate_ops": s.accelerate_ops, "preemptive_ops": s.preemptive_ops,
            "accelerate_samples": s.accelerate_samples, "preemptive_samples": s.preemptive_samples,
            "talkspurts": s.talkspurts, "dtx_refreshes": s.dtx_refreshes, "prebuffers": s.prebuffers,
            "target_delay_ms": s.target_delay_ms, "peak_delay_ms": s.peak_delay_ms, "decode_errors": s.decode_errors,
        }))?,
    )?;
    Ok(())
}

fn cmd_codec(a: &[String]) -> Result<()> {
    let input = read_f32(&a[0])?;
    let kbps: i32 = a[2].parse()?;
    let cfg = VoiceEncoderConfig { bitrate_bps: kbps * 1000, ..VoiceEncoderConfig::default() };
    let packets = encode_stream(&input, cfg)?;
    let sched = schedule(&packets, START_US, 0.0, |_, _| Fate::Deliver { net_delay_us: 0, local_us: 0 });
    let end = sched.last().map(|e| e.local_us).unwrap_or(0) + TAIL_US;
    let mut rx = PlayoutReceiver::new()?;
    let mut rc = RunConfig::until(end);
    rc.start_us = START_US;
    let r = run(&mut rx, &sched, &rc);
    write_f32(&a[1], &r.output)?;
    let voiced: Vec<&VoicePacket> = packets.iter().filter(|p| !p.payload.is_empty()).collect();
    let mut bw: std::collections::BTreeMap<&str, usize> = Default::default();
    for p in &voiced {
        *bw.entry(toc_bandwidth(p.payload[0])).or_default() += 1;
    }
    let bytes: usize = voiced.iter().map(|p| p.payload.len()).sum();
    let s = rx.stats();
    fs::write(
        &a[3],
        serde_json::to_string_pretty(&json!({
            "kbps_set": kbps, "opus": voice_core::opus_version(),
            "packets_total": packets.len(), "packets_voiced": voiced.len(),
            "mean_bytes_per_voiced_packet": bytes as f64 / voiced.len().max(1) as f64,
            "effective_kbps_voiced": bytes as f64 * 8.0 / (voiced.len().max(1) as f64 * 0.02) / 1000.0,
            "toc_bandwidth_counts": bw,
            "toc_bytes_distinct": voiced.iter().map(|p| p.payload[0]).collect::<std::collections::BTreeSet<_>>(),
            "plc_samples": s.plc_samples, "packets_in": s.packets_in,
            "slot_samples": SLOT_SAMPLES,
        }))?,
    )?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pkts") if args.len() == 6 => cmd_pkts(&args[1..]),
        Some("play") if args.len() == 4 => cmd_play(&args[1..]),
        Some("codec") if args.len() == 5 => cmd_codec(&args[1..]),
        _ => bail!("usage: pkts IN.f32 OUT.events KBPS LOSS_PCT SEED | play IN.events OUT.f32 STATS.json | codec IN.f32 OUT.f32 KBPS STATS.json"),
    }
}

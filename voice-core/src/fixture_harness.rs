//! Offline, simulated-clock impairment runs of a receiver. Test and example support only
//! (`test-support` feature).
//!
//! There is no `Instant::now` and no sleeping: time is a `u64` of microseconds that the loop
//! advances itself. Packet arrival times are derived from the packets' media timestamps, so DTX
//! gaps have exactly the spacing the encoder produced; a sender clock offset in ppm scales
//! them.

use crate::encoder::{VoiceEncoder, VoiceEncoderConfig};
use crate::packet::VoicePacket;
use crate::playout::{PacketObservation, PlayoutReceiver};
use crate::Result;

pub const SAMPLE_RATE: u32 = 48_000;
pub const SLOT_SAMPLES: usize = 960;
pub const PULL_SAMPLES: usize = 480;
pub const PULL_PERIOD_US: u64 = 10_000;

/// What the simulator needs from a receiver.
pub trait SimReceiver {
    fn push(&mut self, packet: VoicePacket, daemon_arrival_us: u64, local_us: u64);
    fn pull(&mut self, out: &mut [f32]);
    fn trace_point(&self) -> TracePoint {
        TracePoint::default()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TracePoint {
    pub buffer_ms: f64,
    pub target_ms: f64,
    pub playout_timestamp: Option<u32>,
}

impl SimReceiver for PlayoutReceiver {
    fn push(&mut self, packet: VoicePacket, daemon_arrival_us: u64, local_us: u64) {
        self.push_packet(
            packet,
            PacketObservation {
                daemon_arrival_us,
                local_us,
            },
        );
    }

    fn pull(&mut self, out: &mut [f32]) {
        PlayoutReceiver::pull(self, out);
    }

    fn trace_point(&self) -> TracePoint {
        let s = self.stats();
        TracePoint {
            buffer_ms: s.buffer_ms,
            target_ms: s.target_delay_ms,
            playout_timestamp: s.playout_timestamp,
        }
    }
}

/// A packet scheduled to reach the receiver.
#[derive(Debug, Clone)]
pub struct Scheduled {
    pub packet: VoicePacket,
    pub daemon_us: u64,
    pub local_us: u64,
}

/// Encode `input` (48 kHz mono) as the sender would: one packet per voiced 20 ms frame, DTX
/// gaps advancing only the timestamp, END markers.
pub fn encode_stream(input: &[f32], config: VoiceEncoderConfig) -> Result<Vec<VoicePacket>> {
    let mut encoder = VoiceEncoder::new(config)?;
    let mut packets = Vec::new();
    for chunk in input.chunks(SLOT_SAMPLES) {
        let mut frame = chunk.to_vec();
        frame.resize(SLOT_SAMPLES, 0.0);
        encoder.push_pcm(&frame);
        if let Some(packet) = encoder.poll_packet()? {
            packets.push(packet);
        }
    }
    // The sender stops: the END marker `finish_talkspurt` produces, if it was speaking.
    if let Some(end) = encoder.finish_talkspurt() {
        packets.push(end);
    }
    Ok(packets)
}

/// Timestamp as sender time in microseconds, for a sender clock `ppm` parts per million slow
/// (positive) or fast (negative) against the receiver's.
pub fn sender_time_us(ts: u32, first_ts: u32, ppm: f64) -> f64 {
    let samples = ts.wrapping_sub(first_ts) as f64;
    samples / 0.048 * (1.0 + ppm * 1e-6)
}

/// What the network does to one packet.
#[derive(Debug, Clone, Copy)]
pub enum Fate {
    Drop,
    /// Deliver after this much extra delay (on top of the base), and this much local delay
    /// between the daemon and this process's reader.
    Deliver { net_delay_us: u64, local_us: u64 },
}

/// Schedule `packets`: the first arrives at `start_us` plus its delay.
pub fn schedule(
    packets: &[VoicePacket],
    start_us: u64,
    sender_ppm: f64,
    mut fate: impl FnMut(usize, &VoicePacket) -> Fate,
) -> Vec<Scheduled> {
    let Some(first) = packets.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        if let Fate::Deliver {
            net_delay_us,
            local_us,
        } = fate(i, p)
        {
            let nominal = start_us as f64 + sender_time_us(p.timestamp, first.timestamp, sender_ppm);
            let daemon_us = nominal as u64 + net_delay_us;
            out.push(Scheduled {
                packet: p.clone(),
                daemon_us,
                local_us: daemon_us + local_us,
            });
        }
    }
    out.sort_by_key(|e| e.local_us);
    out
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub pull_samples: usize,
    pub pull_period_us: u64,
    pub start_us: u64,
    pub end_us: u64,
}

impl RunConfig {
    pub fn until(end_us: u64) -> Self {
        Self {
            pull_samples: PULL_SAMPLES,
            pull_period_us: PULL_PERIOD_US,
            start_us: 0,
            end_us,
        }
    }
}

pub struct Run {
    pub output: Vec<f32>,
    /// One point per pull, taken just before it.
    pub trace: Vec<TracePoint>,
}

/// Drive `rx` with `events` (sorted by `local_us`) and pulls on a fixed period.
pub fn run<R: SimReceiver>(rx: &mut R, events: &[Scheduled], cfg: &RunConfig) -> Run {
    let mut output = Vec::new();
    let mut trace = Vec::new();
    let mut next = 0;
    let mut t = cfg.start_us;
    let mut frame = vec![0.0; cfg.pull_samples];
    while t <= cfg.end_us {
        while next < events.len() && events[next].local_us <= t {
            let e = &events[next];
            rx.push(e.packet.clone(), e.daemon_us, e.local_us);
            next += 1;
        }
        trace.push(rx.trace_point());
        rx.pull(&mut frame);
        output.extend_from_slice(&frame);
        t += cfg.pull_period_us;
    }
    Run { output, trace }
}

// ---- signals and measurements ----------------------------------------------------------------

/// A voiced, speech-like signal: a harmonic series on a wandering pitch under a syllabic
/// envelope that never reaches zero (so Opus keeps sending frames).
pub fn voiced_signal(seconds: f32, amplitude: f32) -> Vec<f32> {
    let n = (seconds * SAMPLE_RATE as f32) as usize;
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            let f0 = 150.0 + 25.0 * (2.0 * std::f64::consts::PI * 0.7 * t).sin();
            phase += 2.0 * std::f64::consts::PI * f0 / SAMPLE_RATE as f64;
            let env = 0.55 + 0.45 * (2.0 * std::f64::consts::PI * 3.3 * t).sin();
            let mut v = 0.0;
            for k in 1..=10 {
                v += (phase * k as f64).sin() / k as f64;
            }
            (amplitude as f64 * env * v * 0.5) as f32
        })
        .collect()
}

/// A tone whose frequency rises steadily from `f_start` to `f_end`, so the dominant frequency
/// of a stretch of output says where in the media timeline it came from.
pub fn chirp(seconds: f32, f_start: f64, f_end: f64, amplitude: f32) -> Vec<f32> {
    let n = (seconds * SAMPLE_RATE as f32) as usize;
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let frac = i as f64 / n as f64;
            let f = f_start + (f_end - f_start) * frac;
            phase += 2.0 * std::f64::consts::PI * f / SAMPLE_RATE as f64;
            amplitude * phase.sin() as f32
        })
        .collect()
}

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Largest absolute difference between consecutive samples in `x`.
pub fn max_step(x: &[f32]) -> f32 {
    x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

/// Frequency of the strongest of a Goertzel bank over `freqs` in `x`.
pub fn dominant_frequency(x: &[f32], freqs: &[f64]) -> f64 {
    let mut best = (0.0, freqs[0]);
    for &f in freqs {
        let w = 2.0 * std::f64::consts::PI * f / SAMPLE_RATE as f64;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        let c = 2.0 * w.cos();
        for &v in x {
            let s0 = v as f64 + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let p = s1 * s1 + s2 * s2 - c * s1 * s2;
        if p > best.0 {
            best = (p, f);
        }
    }
    best.1
}

//! The Prim-owned receive path: one [`PlayoutReceiver`] per peer.
//!
//! Playback is driven by a *media timeline*, not by a queue of whatever arrived. The timeline is
//! a sequence of fixed 20 ms slots on the sender's 48 kHz clock (`timestamp += 960`); the cursor
//! is the media timestamp of the next slot to play. For each slot:
//!
//! 1. its packet is in the buffer: decode it;
//! 2. it is missing and the next slot's packet carries in-band FEC (and FEC is enabled): recover
//!    it from that packet;
//! 3. otherwise it is lost: Opus packet-loss concealment (`decode(None)`), at most
//!    [`PlayoutConfig::plc_max_ms`] in a row, then digital silence.
//!
//! A packet whose timestamp is behind the cursor is counted (`late`) and dropped, so nothing
//! ever plays after audio of a newer timestamp. `timestamp` is the only clock; `seq` is used
//! only to tell loss (a hole in `seq`) from DTX (a hole in `timestamp` with `seq` contiguous).
//! Talkspurt START/END flags are hints: an END marker lets the receiver stop concealing at
//! once, but nothing depends on it.
//!
//! The receiver owns no clock. Every time it sees (`daemon_arrival_us`, `local_us`) is passed in
//! by the caller; with injected times a simulation is exact (see `fixture_harness`).
//!
//! Jitter handling: a [`DelayEstimator`] (cumulative relative delay, peak tracker) sets a target
//! playout delay; the buffer level is steered to it with Accelerate / Preemptive Expand at the
//! 20 ms boundary, only on cleanly decoded audio and not in the first moments of a talkspurt.

use std::collections::{BTreeMap, VecDeque};

use crate::decoder::{payload_has_fec, validate_payload, OpusAudioDecoder, SLOT_SAMPLES};
use crate::delay::DelayEstimator;
use crate::error::Result;
use crate::packet::{PacketFlags, VoicePacket};
use crate::time_stretch::{Accelerate, PreemptiveExpand, TimeStretchResult, TimeStretcher};

const SLOT: i64 = SLOT_SAMPLES as i64;
const SAMPLES_PER_MS: i64 = 48;
/// Longest run of slots kept buffered before the oldest are dropped, as a guard on memory.
const MAX_BUFFERED_PACKETS: usize = 200;
/// Remembered decoded timestamps, to tell a duplicate of a played packet from a late one.
const RECENT_PACKETS: usize = 64;
/// Accelerate / Preemptive Expand read this many samples ahead (30 ms).
const STRETCH_LOOKAHEAD: usize = SLOT_SAMPLES + SLOT_SAMPLES / 2;

#[derive(Debug, Clone)]
pub struct PlayoutConfig {
    /// Lower and upper bound of the target playout delay.
    pub min_delay_ms: u32,
    pub max_delay_ms: u32,
    /// Use in-band FEC of the next packet to recover a single lost slot. Leave off unless the
    /// sender encodes with `inband_fec`.
    pub fec: bool,
    /// Consecutive slots of concealment after the last real packet before going silent.
    pub plc_max_ms: u32,
    /// Added to the peak relative delay to form the target.
    pub safety_margin_ms: u32,
    /// Level control is off for this long after a talkspurt starts.
    pub stretch_holdoff_ms: u32,
    /// Level control (Accelerate / Preemptive Expand). Off means the delay is whatever the
    /// talkspurt start gave it.
    pub time_stretch: bool,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        Self {
            min_delay_ms: 20,
            max_delay_ms: 250,
            fec: false,
            plc_max_ms: 120,
            safety_margin_ms: 10,
            stretch_holdoff_ms: 160,
            time_stretch: true,
        }
    }
}

/// The two times the receiver is given for each packet, microseconds on one monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketObservation {
    /// When the daemon read the datagram off the network (its arrival stamp). The delay
    /// estimator uses this: it is the network's delay.
    pub daemon_arrival_us: u64,
    /// When the reader in this process got it. Kept separately: lateness that only exists here
    /// is local scheduling, counted as `local_late`.
    pub local_us: u64,
}

impl PacketObservation {
    pub fn same(us: u64) -> Self {
        Self {
            daemon_arrival_us: us,
            local_us: us,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Buffered for playout.
    Queued,
    /// An END-of-talkspurt marker (no audio).
    Marker,
    /// Arrived out of order but still ahead of the cursor; buffered.
    Reordered,
    /// The same timestamp was already buffered or played.
    Duplicate,
    /// Behind the cursor: its slot has been played (or concealed). Dropped.
    Late,
    /// Not exactly one 20 ms 48 kHz mono Opus frame. Dropped.
    Rejected,
}

#[derive(Debug, Clone, Default)]
pub struct PlayoutStats {
    /// Samples handed out as audio: decoded, FEC-recovered, concealed or time-stretched.
    pub output_samples: u64,
    /// Of `output_samples`, produced by Opus PLC.
    pub plc_samples: u64,
    /// Of `output_samples`, recovered from the next packet's in-band FEC.
    pub fec_samples: u64,
    /// Digital silence inside a talkspurt's timeline (a DTX gap or lost run past the PLC bound).
    pub silence_samples: u64,
    /// Zeros handed out while idle (no talkspurt).
    pub idle_samples: u64,
    pub packets_in: u64,
    pub packets_decoded: u64,
    pub markers: u64,
    pub late: u64,
    pub duplicate: u64,
    pub reordered_accepted: u64,
    pub rejected: u64,
    /// Late packets whose daemon-side delay was within the target: the lateness was local.
    pub local_late: u64,
    /// Largest `local_us - daemon_arrival_us` seen, milliseconds.
    pub local_delay_max_ms: f64,
    pub accelerate_samples: u64,
    pub preemptive_samples: u64,
    pub accelerate_ops: u64,
    pub preemptive_ops: u64,
    /// Talkspurts with at least two decoded packets.
    pub talkspurts: u64,
    /// Single-packet runs: DTX comfort-noise refreshes.
    pub dtx_refreshes: u64,
    /// Talkspurts that started after waiting for the buffer to fill to the target.
    pub prebuffers: u64,
    /// Times the delay estimate was reset (a new stream), never by a talkspurt or a refresh.
    pub target_resets: u64,
    /// Samples of media discarded to get back under `max_delay_ms` after a delay step.
    pub skipped_samples: u64,
    pub overflow_dropped: u64,
    pub target_delay_ms: f64,
    pub peak_delay_ms: f64,
    /// Media buffered ahead of the playout position, milliseconds.
    pub buffer_ms: f64,
    /// Media timestamp of the next sample to be pulled; `None` while idle.
    pub playout_timestamp: Option<u32>,
    pub idle: bool,
    pub decode_errors: u64,
}

impl PlayoutStats {
    /// Concealed (PLC) share of the audio produced, in percent.
    pub fn concealed_pct(&self) -> f64 {
        if self.output_samples == 0 {
            0.0
        } else {
            100.0 * self.plc_samples as f64 / self.output_samples as f64
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    seq: u16,
    /// Empty for an END marker.
    payload: Vec<u8>,
    has_fec: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Normal,
    Fec,
    Plc,
    Silence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Playing,
}

pub struct PlayoutReceiver {
    cfg: PlayoutConfig,
    dec: OpusAudioDecoder,
    est: DelayEstimator,
    accel: Accelerate,
    pexp: PreemptiveExpand,

    // Packet side.
    buf: BTreeMap<i64, Entry>,
    recent: VecDeque<i64>,
    highest_ext: Option<i64>,
    /// Unwrapping anchor: the last timestamp accepted, as an extended value.
    anchor: Option<i64>,

    // Timeline.
    state: State,
    /// Extended timestamp of the next slot to decode (Playing); the first timestamp that is
    /// still acceptable (Idle).
    cursor: i64,
    /// Where the previous entry's coverage ended, and its `seq`, for the loss-or-DTX test.
    last_end: i64,
    last_seq: u16,
    /// First silent timestamp announced by an END marker.
    end_ext: Option<i64>,
    onset_wait_samples: usize,
    onset_waited: bool,
    run_packets: u32,
    run_counted: bool,
    silence_run: u32,
    conceal_run: u32,
    slots_in_run: u32,

    // Decoded samples not yet turned into output, and their kinds, in media order. They end at
    // `cursor`.
    raw: Vec<f32>,
    raw_kinds: VecDeque<(usize, Kind)>,
    // Output block being drained by `pull`.
    out: Vec<f32>,
    out_pos: usize,
    out_ts: i64,
    out_media_len: usize,

    stats: PlayoutStats,
}

impl PlayoutReceiver {
    pub fn new() -> Result<Self> {
        Self::with_config(PlayoutConfig::default())
    }

    pub fn new_with_delay_bounds(min_delay_ms: u32, max_delay_ms: u32) -> Result<Self> {
        Self::with_config(PlayoutConfig {
            min_delay_ms,
            max_delay_ms,
            ..PlayoutConfig::default()
        })
    }

    pub fn with_config(cfg: PlayoutConfig) -> Result<Self> {
        Ok(Self {
            est: DelayEstimator::new(cfg.min_delay_ms, cfg.max_delay_ms, cfg.safety_margin_ms),
            cfg,
            dec: OpusAudioDecoder::new()?,
            accel: Accelerate::new(48_000, 1),
            pexp: PreemptiveExpand::new(48_000, 1),
            buf: BTreeMap::new(),
            recent: VecDeque::new(),
            highest_ext: None,
            anchor: None,
            state: State::Idle,
            cursor: i64::MIN,
            last_end: 0,
            last_seq: 0,
            end_ext: None,
            onset_wait_samples: 0,
            onset_waited: false,
            run_packets: 0,
            run_counted: false,
            silence_run: 0,
            conceal_run: 0,
            slots_in_run: 0,
            raw: Vec::new(),
            raw_kinds: VecDeque::new(),
            out: Vec::new(),
            out_pos: 0,
            out_ts: 0,
            out_media_len: 0,
            stats: PlayoutStats::default(),
        })
    }

    pub fn config(&self) -> &PlayoutConfig {
        &self.cfg
    }

    /// Offer one packet. Validates it first; nothing is counted for a rejected packet except
    /// `rejected`.
    pub fn push_packet(&mut self, pkt: VoicePacket, obs: PacketObservation) -> PushOutcome {
        let is_marker = pkt.payload.is_empty();
        if is_marker {
            if !pkt.flags.contains(PacketFlags::END_OF_TALKSPURT) {
                self.stats.rejected += 1;
                return PushOutcome::Rejected;
            }
        } else if validate_payload(&pkt.payload).is_err() {
            self.stats.rejected += 1;
            return PushOutcome::Rejected;
        }

        self.stats.packets_in += 1;
        let local_delay_ms = obs.local_us.saturating_sub(obs.daemon_arrival_us) as f64 / 1000.0;
        if local_delay_ms > self.stats.local_delay_max_ms {
            self.stats.local_delay_max_ms = local_delay_ms;
        }

        let ext = self.unwrap_ts(pkt.timestamp);

        if is_marker {
            self.stats.markers += 1;
            // A marker behind the cursor describes a slot already settled.
            if self.cursor_known() && ext < self.cursor {
                return PushOutcome::Marker;
            }
            if self.buf.contains_key(&ext) {
                return PushOutcome::Duplicate;
            }
            self.buf.insert(
                ext,
                Entry {
                    seq: pkt.seq,
                    payload: Vec::new(),
                    has_fec: false,
                },
            );
            self.end_ext = Some(self.end_ext.map_or(ext, |e| e.max(ext)));
            return PushOutcome::Marker;
        }

        // The estimator sees every distinct packet, late ones included: they are exactly the
        // evidence that the buffer was too shallow.
        let duplicate = self.buf.contains_key(&ext) || self.recent.contains(&ext);
        let target_before = self.est.target_ms();
        if !duplicate {
            let rel_us = self.est.update(obs.daemon_arrival_us, ext);
            if self.cursor_known() && ext < self.cursor {
                if (rel_us as f64 / 1000.0) <= target_before {
                    self.stats.local_late += 1;
                }
            }
        }
        if duplicate {
            self.stats.duplicate += 1;
            return PushOutcome::Duplicate;
        }
        if self.cursor_known() && ext < self.cursor {
            self.stats.late += 1;
            return PushOutcome::Late;
        }

        let reordered = self.highest_ext.is_some_and(|h| ext < h);
        self.highest_ext = Some(self.highest_ext.map_or(ext, |h| h.max(ext)));
        let has_fec = self.cfg.fec && payload_has_fec(&pkt.payload);
        self.buf.insert(
            ext,
            Entry {
                seq: pkt.seq,
                payload: pkt.payload,
                has_fec,
            },
        );
        while self.buf.len() > MAX_BUFFERED_PACKETS {
            let oldest = *self.buf.keys().next().expect("non-empty");
            self.buf.remove(&oldest);
            self.stats.overflow_dropped += 1;
        }
        if reordered {
            self.stats.reordered_accepted += 1;
            PushOutcome::Reordered
        } else {
            PushOutcome::Queued
        }
    }

    /// Fill `out` with the next samples of the voice, any length. Digital silence while idle.
    pub fn pull(&mut self, out: &mut [f32]) {
        let mut filled = 0;
        while filled < out.len() {
            if self.out_pos >= self.out.len() && !self.make_block() {
                break;
            }
            let n = (self.out.len() - self.out_pos).min(out.len() - filled);
            out[filled..filled + n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
            self.out_pos += n;
            filled += n;
        }
        if filled < out.len() {
            let rest = out.len() - filled;
            out[filled..].fill(0.0);
            self.stats.idle_samples += rest as u64;
            if self.state == State::Idle && !self.buf.is_empty() {
                self.onset_wait_samples += rest;
            }
        }
    }

    /// Media timestamp (48 kHz, wrapping) of the first sample the next [`pull`](Self::pull) will
    /// return, or `None` while idle.
    ///
    /// It is the position in the sender's timeline of the sample about to leave this receiver:
    /// no output-device or mixer latency is subtracted, and inside a time-stretched block it
    /// advances proportionally (a block that compresses 1000 media samples into 960 output
    /// samples moves it 1000 / 960 per sample).
    pub fn playout_timestamp(&self) -> Option<u32> {
        if self.out_pos < self.out.len() {
            let frac = (self.out_pos as u64 * self.out_media_len as u64) / self.out.len() as u64;
            return Some((self.out_ts + frac as i64) as u32);
        }
        if self.state == State::Playing || !self.raw.is_empty() {
            return Some((self.cursor - self.raw.len() as i64) as u32);
        }
        None
    }

    /// True when the receiver is in digital silence with nothing buffered, so the caller may
    /// skip pulling, decoding-side work and HRTF rendering for this peer until the next packet.
    /// (Pulling while idle is still valid and returns zeros.)
    pub fn is_idle(&self) -> bool {
        self.state == State::Idle
            && self.out_pos >= self.out.len()
            && !self.buf.values().any(|e| !e.payload.is_empty())
    }

    pub fn stats(&self) -> PlayoutStats {
        let mut s = self.stats.clone();
        s.target_delay_ms = self.est.target_ms();
        s.peak_delay_ms = self.est.peak_ms();
        s.playout_timestamp = self.playout_timestamp();
        s.idle = self.state == State::Idle;
        s.buffer_ms = match (self.state, self.highest_ext) {
            (State::Playing, Some(h)) => {
                ((h + SLOT - (self.cursor - self.raw.len() as i64)).max(0)) as f64
                    / SAMPLES_PER_MS as f64
            }
            _ => 0.0,
        };
        s
    }

    /// Forget everything about the current stream (a new incarnation). Cumulative counters stay.
    pub fn reset_stream(&mut self) -> Result<()> {
        self.dec.reset()?;
        self.est.reset();
        self.accel.reset();
        self.pexp.reset();
        self.buf.clear();
        self.recent.clear();
        self.highest_ext = None;
        self.anchor = None;
        self.state = State::Idle;
        self.cursor = i64::MIN;
        self.end_ext = None;
        self.onset_wait_samples = 0;
        self.onset_waited = false;
        self.run_packets = 0;
        self.run_counted = false;
        self.silence_run = 0;
        self.conceal_run = 0;
        self.slots_in_run = 0;
        self.raw.clear();
        self.raw_kinds.clear();
        self.out.clear();
        self.out_pos = 0;
        self.stats.target_resets += 1;
        Ok(())
    }

    // ---- timeline -------------------------------------------------------------------------

    fn cursor_known(&self) -> bool {
        self.cursor != i64::MIN
    }

    /// Extend a wrapping timestamp to an `i64` near the previously seen ones.
    fn unwrap_ts(&mut self, ts: u32) -> i64 {
        let ext = match self.anchor {
            None => ts as i64,
            Some(a) => a + (ts.wrapping_sub(a as u32) as i32) as i64,
        };
        // Follow the highest value so a long run of in-order packets keeps the anchor close.
        self.anchor = Some(self.anchor.map_or(ext, |a| a.max(ext)));
        ext
    }

    /// The buffer delay the level control aims for, in whole slots of media between the
    /// playout position and the end of the newest packet: `floor(target / 20 ms) + 1`.
    fn target_slots(&self) -> i64 {
        (self.est.target_ms() / 20.0).floor() as i64 + 2
    }

    fn make_block(&mut self) -> bool {
        if self.state == State::Idle {
            if !self.raw.is_empty() {
                // Decoded audio left over from before the talkspurt ended.
                let start_ts = self.cursor - self.raw.len() as i64;
                return self.emit_plain_block(start_ts);
            }
            if !self.try_start() {
                return false;
            }
        }
        self.skip_excess_delay();
        let level_slots = self.level_slots();
        let want_stretch = self.cfg.time_stretch
            && self.slots_in_run >= self.cfg.stretch_holdoff_ms / 20
            && self.conceal_run == 0;
        let target = self.target_slots();

        // Accelerate: well above target. Preemptive expand: below it.
        let mut stretch = None;
        if want_stretch {
            if level_slots >= target + 2 {
                stretch = Some(level_slots >= target + 4);
            } else if self.span_samples() < target * SLOT {
                // Unrounded: a stretch needs 1.5 slots of audio in hand, so it has to fire
                // while the buffer is still between 1.5 and `target` slots deep.
                stretch = Some(false);
            }
        }

        // Decode far enough ahead for the block. A stretch needs 30 ms of cleanly decoded
        // audio; if the next slot isn't a real packet, no stretch.
        self.fill_raw(SLOT_SAMPLES, false);
        let mut do_accel = false;
        let mut do_expand = false;
        if stretch.is_some() {
            if self.raw_prefix_normal(SLOT_SAMPLES) {
                self.fill_raw(STRETCH_LOOKAHEAD, true);
                if self.raw.len() >= STRETCH_LOOKAHEAD && self.raw_prefix_normal(STRETCH_LOOKAHEAD)
                {
                    if level_slots >= target + 2 {
                        do_accel = true;
                    } else {
                        do_expand = true;
                    }
                }
            }
        }

        if self.raw.is_empty() {
            // Went idle while looking for the next slot.
            return false;
        }
        let start_ts = self.cursor - self.raw.len() as i64;
        if do_accel {
            let fast = level_slots >= target + 4;
            let mut block = vec![0.0f32; SLOT_SAMPLES];
            let res = self.accel.process(&self.raw[..STRETCH_LOOKAHEAD], &mut block, fast);
            let used = self.accel.get_used_input_samples();
            if matches!(res, TimeStretchResult::NoStretch) || used <= SLOT_SAMPLES || used > STRETCH_LOOKAHEAD
            {
                return self.emit_plain_block(start_ts);
            }
            self.stats.accelerate_samples += (used - SLOT_SAMPLES) as u64;
            self.stats.accelerate_ops += 1;
            self.account_output(block.len(), used, Kind::Normal);
            self.consume_raw(used);
            self.set_block(block, start_ts, used);
            return true;
        }
        if do_expand {
            let mut block = vec![0.0f32; STRETCH_LOOKAHEAD];
            let res = self.pexp.process(&self.raw[..STRETCH_LOOKAHEAD], &mut block, false);
            let used = self.pexp.get_used_input_samples();
            if matches!(res, TimeStretchResult::NoStretch) || used >= STRETCH_LOOKAHEAD || used < SLOT_SAMPLES
            {
                return self.emit_plain_block(start_ts);
            }
            self.stats.preemptive_samples += (STRETCH_LOOKAHEAD - used) as u64;
            self.stats.preemptive_ops += 1;
            self.account_output(block.len(), used, Kind::Normal);
            self.consume_raw(used);
            self.set_block(block, start_ts, used);
            return true;
        }
        self.emit_plain_block(start_ts)
    }

    fn emit_plain_block(&mut self, start_ts: i64) -> bool {
        let n = self.raw.len().min(SLOT_SAMPLES);
        let block: Vec<f32> = self.raw[..n].to_vec();
        self.account_prefix(n);
        self.consume_raw(n);
        self.set_block(block, start_ts, n);
        true
    }

    fn set_block(&mut self, block: Vec<f32>, ts: i64, media_len: usize) {
        self.out = block;
        self.out_pos = 0;
        self.out_ts = ts;
        self.out_media_len = media_len;
    }

    fn consume_raw(&mut self, n: usize) {
        self.raw.drain(..n);
        let mut left = n;
        while left > 0 {
            let Some(front) = self.raw_kinds.front_mut() else {
                break;
            };
            if front.0 <= left {
                left -= front.0;
                self.raw_kinds.pop_front();
            } else {
                front.0 -= left;
                left = 0;
            }
        }
    }

    fn raw_prefix_normal(&self, n: usize) -> bool {
        let mut left = n;
        for &(len, kind) in &self.raw_kinds {
            if kind != Kind::Normal {
                return false;
            }
            if len >= left {
                return true;
            }
            left -= len;
        }
        left == 0
    }

    /// Count the first `n` raw samples into the stats by kind.
    fn account_prefix(&mut self, n: usize) {
        let mut left = n;
        let kinds: Vec<(usize, Kind)> = self.raw_kinds.iter().copied().collect();
        for (len, kind) in kinds {
            if left == 0 {
                break;
            }
            let take = len.min(left);
            self.account_output(take, take, kind);
            left -= take;
        }
    }

    fn account_output(&mut self, out_samples: usize, _media: usize, kind: Kind) {
        let n = out_samples as u64;
        match kind {
            Kind::Normal => self.stats.output_samples += n,
            Kind::Fec => {
                self.stats.output_samples += n;
                self.stats.fec_samples += n;
            }
            Kind::Plc => {
                self.stats.output_samples += n;
                self.stats.plc_samples += n;
            }
            Kind::Silence => self.stats.silence_samples += n,
        }
    }

    /// Slots of media from the playout position to the end of the newest packet.
    /// Audio in hand, in samples: from the next sample to be output to the end of the newest
    /// packet.
    fn span_samples(&self) -> i64 {
        match self.highest_ext {
            Some(h) => (h + SLOT - (self.cursor - self.raw.len() as i64)).max(0),
            None => 0,
        }
    }

    fn level_slots(&self) -> i64 {
        match self.highest_ext {
            Some(h) => {
                let pos = self.cursor - self.raw.len() as i64;
                ((h + SLOT - pos).max(0) + SLOT / 2) / SLOT
            }
            None => 0,
        }
    }

    /// After a lasting delay step the buffer can hold more than `max_delay_ms`; jump forward
    /// rather than play it all.
    fn skip_excess_delay(&mut self) {
        let Some(h) = self.highest_ext else { return };
        let pos = self.cursor - self.raw.len() as i64;
        let max = self.cfg.max_delay_ms as i64 * SAMPLES_PER_MS + 2 * SLOT;
        if h - pos <= max {
            return;
        }
        // Resume half way into the allowed delay, on a packet boundary.
        let want = h - self.cfg.max_delay_ms as i64 * SAMPLES_PER_MS / 2;
        let Some((&to, e)) = self.buf.range(want..).next() else {
            return;
        };
        let to_seq = e.seq;
        self.stats.skipped_samples += (to - pos).max(0) as u64;
        let keys: Vec<i64> = self.buf.range(..to).map(|(k, _)| *k).collect();
        for k in keys {
            self.buf.remove(&k);
        }
        self.raw.clear();
        self.raw_kinds.clear();
        self.cursor = to;
        self.last_end = to;
        self.last_seq = to_seq.wrapping_sub(1);
        self.conceal_run = 0;
    }

    // ---- talkspurt start --------------------------------------------------------------------

    /// Idle with packets buffered: start playing once the buffer holds the target, or when the
    /// wait for it has run its course. Returns whether playback began.
    fn try_start(&mut self) -> bool {
        let Some((&first, entry)) = self.buf.iter().find(|(_, e)| !e.payload.is_empty()) else {
            return false;
        };
        let first_seq = entry.seq;
        let newest = self
            .buf
            .iter()
            .rev()
            .find(|(_, e)| !e.payload.is_empty())
            .map(|(k, _)| *k)
            .unwrap_or(first);
        let span_slots = (newest - first) / SLOT + 1;
        let n_start = self.target_slots();
        let waited_enough = self.onset_wait_samples as i64 >= (n_start - 1) * SLOT;
        if span_slots < n_start && !waited_enough {
            return false;
        }
        self.onset_waited = n_start > 1 && self.onset_wait_samples > 0;
        // Markers older than the first packet belong to the previous talkspurt.
        let stale: Vec<i64> = self.buf.range(..first).map(|(k, _)| *k).collect();
        for k in stale {
            self.buf.remove(&k);
        }
        self.state = State::Playing;
        self.cursor = first;
        self.last_end = first;
        self.last_seq = first_seq.wrapping_sub(1);
        self.end_ext = self.end_ext.filter(|&e| e > first);
        self.run_packets = 0;
        self.run_counted = false;
        self.silence_run = 0;
        self.conceal_run = 0;
        self.slots_in_run = 0;
        self.raw.clear();
        self.raw_kinds.clear();
        true
    }

    // ---- per-slot production ------------------------------------------------------------------

    /// Decode slots until `raw` holds at least `want` samples. With `real_only`, stop at the
    /// first slot that has no packet of its own (nothing is guessed ahead of time). Going idle
    /// stops it as well.
    fn fill_raw(&mut self, want: usize, real_only: bool) {
        while self.raw.len() < want && self.state == State::Playing {
            if real_only && !self.buf.get(&self.cursor).is_some_and(|e| !e.payload.is_empty()) {
                break;
            }
            self.decode_next_slot();
        }
    }

    fn push_raw(&mut self, slot: &[f32], kind: Kind) {
        self.raw.extend_from_slice(slot);
        match self.raw_kinds.back_mut() {
            Some(last) if last.1 == kind => last.0 += slot.len(),
            _ => self.raw_kinds.push_back((slot.len(), kind)),
        }
        self.cursor += SLOT;
        self.slots_in_run += 1;
    }

    fn decode_next_slot(&mut self) {
        let c = self.cursor;
        // Anything the cursor has passed (a marker, or what a skip left behind) is settled.
        let stale: Vec<i64> = self.buf.range(..c).map(|(k, _)| *k).collect();
        for k in stale {
            self.buf.remove(&k);
        }

        if let Some(entry) = self.buf.remove(&c) {
            if entry.payload.is_empty() {
                // An END marker sits at the slot being decided: silence starts here. It uses a
                // `seq` but no audio.
                self.last_end = c;
                self.last_seq = entry.seq;
                self.end_ext = Some(self.end_ext.map_or(c, |e| e.min(c)));
                self.decide_missing(c);
                return;
            }
            let mut slot = [0.0f32; SLOT_SAMPLES];
            if self.dec.decode(&entry.payload, &mut slot).is_err() {
                self.stats.decode_errors += 1;
                slot.fill(0.0);
            }
            if self.silence_run >= 2 {
                self.close_run();
            }
            self.silence_run = 0;
            self.conceal_run = 0;
            self.run_packets += 1;
            if self.run_packets == 2 && !self.run_counted {
                self.run_counted = true;
                self.stats.talkspurts += 1;
                if self.onset_waited {
                    self.stats.prebuffers += 1;
                }
            }
            self.stats.packets_decoded += 1;
            self.recent.push_back(c);
            if self.recent.len() > RECENT_PACKETS {
                self.recent.pop_front();
            }
            self.last_end = c + SLOT;
            self.last_seq = entry.seq;
            if self.end_ext.is_some_and(|e| e <= c) {
                self.end_ext = None;
            }
            self.push_raw(&slot, Kind::Normal);
            return;
        }

        self.decide_missing(c);
    }

    /// Slot `c` has no packet of its own: recover it from FEC, conceal it, or let it be silent.
    fn decide_missing(&mut self, c: i64) {
        let next = self
            .buf
            .range(c..)
            .next()
            .map(|(k, e)| (*k, e.seq, e.has_fec && !e.payload.is_empty()));

        // In-band FEC from the very next packet.
        if let Some((k, _, true)) = next {
            if self.cfg.fec && k == c + SLOT {
                let payload = self.buf.get(&k).map(|e| e.payload.clone()).unwrap_or_default();
                let mut slot = [0.0f32; SLOT_SAMPLES];
                if self.dec.decode_fec(&payload, &mut slot).is_ok() {
                    if self.silence_run >= 2 {
                        self.close_run();
                    }
                    self.silence_run = 0;
                    self.conceal_run = 0;
                    self.last_end = c + SLOT;
                    self.last_seq = self.last_seq.wrapping_add(1);
                    self.push_raw(&slot, Kind::Fec);
                    return;
                }
            }
        }

        if self.slot_is_loss(next.map(|(k, s, _)| (k, s)))
            && self.conceal_run < self.cfg.plc_max_ms / 20
        {
            let mut slot = [0.0f32; SLOT_SAMPLES];
            if self.dec.conceal(&mut slot).is_err() {
                self.stats.decode_errors += 1;
                slot.fill(0.0);
            }
            self.conceal_run += 1;
            self.silence_run = 0;
            self.last_end = c + SLOT;
            self.last_seq = self.last_seq.wrapping_add(1);
            self.push_raw(&slot, Kind::Plc);
            return;
        }

        // Silence. With a later packet already buffered the timeline keeps running through the
        // gap; with none there is nothing to wait for: go idle.
        if next.is_some() {
            self.silence_run += 1;
            if self.silence_run == 2 {
                self.close_run();
            }
            self.last_end = c + SLOT;
            let slot = [0.0f32; SLOT_SAMPLES];
            self.push_raw(&slot, Kind::Silence);
            // Silence is not part of a stretchable run.
            self.slots_in_run = 0;
        } else {
            self.go_idle(c);
        }
    }

    /// Is the missing slot a lost packet (to be concealed) rather than silence?
    ///
    /// `last_end` / `last_seq` describe the entry (or concealed slot) just before it. With a
    /// later packet `B` buffered, `seq` says how many packets are missing between them; the
    /// first of the missing slots are those packets, any slots beyond that are DTX silence.
    fn slot_is_loss(&self, next: Option<(i64, u16)>) -> bool {
        if self.end_ext.is_some_and(|e| e <= self.last_end) {
            return false;
        }
        match next {
            // Nothing later is known: it may only be late. Conceal (bounded by the caller).
            None => true,
            Some((kb, seq_b)) => {
                if kb <= self.last_end {
                    return false;
                }
                let dist = seq_b.wrapping_sub(self.last_seq);
                if dist == 0 || dist >= 1 << 15 {
                    // `B` is not after our reference in `seq`: reordered; the slot is a hole.
                    return true;
                }
                dist > 1
            }
        }
    }

    fn close_run(&mut self) {
        if self.run_packets == 1 {
            self.stats.dtx_refreshes += 1;
        }
        self.run_packets = 0;
        self.run_counted = false;
        self.onset_waited = false;
        self.slots_in_run = 0;
    }

    fn go_idle(&mut self, at: i64) {
        self.close_run();
        self.state = State::Idle;
        self.cursor = at;
        self.end_ext = None;
        self.onset_wait_samples = 0;
        self.silence_run = 0;
        self.conceal_run = 0;
    }
}

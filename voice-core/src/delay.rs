//! Cumulative (WebRTC style) relative-delay estimator.
//!
//! For each packet the network delay is measured against the fastest packet of the last
//! [`BASELINE_WINDOW_US`]: `rel = d(i) - min d(window)`, with
//! `d(i) = (arrival_i - arrival_0) - (ts_i - ts_0) / 48000`. A clock-rate difference between the
//! two ends moves `d` by at most ~1 ms in the window, so slow drift never accumulates into the
//! estimate (the buffer-level control absorbs it instead).
//!
//! The target is a *peak* tracker over those relative delays: a spike raises it at once and it
//! falls only at [`DECAY_US_PER_S`], so sparse spikes (a Wi-Fi hold once a second, one 80 ms
//! stall) are still in the estimate when the next one comes. The percentile of a short window
//! that this replaces reads 0 whenever spikes are rarer than the window's tail.
//!
//! The peak is bounded: a relative delay is clamped to `max_delay_ms - 2 * margin` before it can
//! raise the peak, and one above `2 * max_delay_ms` (a stray timestamp, a path stalled far beyond
//! what a buffer could cover) is not fed to it at all. So the worst case is a peak of 230 ms
//! with the defaults, back to the minimum 46 s later, whatever one packet claims.
//!
//! All times are in microseconds on the daemon's arrival clock; the estimator has no clock of
//! its own.

use std::collections::VecDeque;

const BASELINE_WINDOW_US: u64 = 2_000_000;
/// The peak falls by this many microseconds per second of arrival time (5 ms/s).
const DECAY_US_PER_S: f64 = 5_000.0;
/// 48 kHz samples per microsecond.
const SAMPLES_PER_US: f64 = 0.048;

#[derive(Debug, Clone)]
pub struct DelayEstimator {
    min_delay_ms: u32,
    max_delay_ms: u32,
    margin_ms: u32,
    /// A relative delay is clamped to this before it can raise the peak, so the target can reach
    /// `max_delay_ms` but the peak can never need more than `peak_cap_us / DECAY` to fall.
    peak_cap_us: f64,
    /// A relative delay above this is an outlier (a stray timestamp, a stalled path): it says
    /// nothing a buffer of `max_delay_ms` could act on, and is not fed to the peak at all.
    reject_us: f64,
    reference: Option<(u64, i64)>,
    /// Monotonic (increasing `d`) deque of `(arrival_us, d_us)`: its front is the window minimum.
    window: VecDeque<(u64, i64)>,
    newest_arrival_us: u64,
    peak_us: f64,
    last_rel_us: i64,
}

impl DelayEstimator {
    pub fn new(min_delay_ms: u32, max_delay_ms: u32, margin_ms: u32) -> Self {
        let max_delay_ms = max_delay_ms.max(min_delay_ms);
        Self {
            min_delay_ms,
            max_delay_ms,
            margin_ms,
            // Twice the margin below the cap: with the defaults 230 ms, which decays in 46 s.
            peak_cap_us: max_delay_ms.saturating_sub(2 * margin_ms).max(min_delay_ms) as f64
                * 1000.0,
            reject_us: 2.0 * max_delay_ms as f64 * 1000.0,
            reference: None,
            window: VecDeque::new(),
            newest_arrival_us: 0,
            peak_us: 0.0,
            last_rel_us: 0,
        }
    }

    pub fn reset(&mut self) {
        self.reference = None;
        self.window.clear();
        self.newest_arrival_us = 0;
        self.peak_us = 0.0;
        self.last_rel_us = 0;
    }

    /// Account one packet and return its delay relative to the fastest packet of the window,
    /// in microseconds. `ext_ts` is the unwrapped 48 kHz media timestamp.
    pub fn update(&mut self, arrival_us: u64, ext_ts: i64) -> i64 {
        let (a0, t0) = *self.reference.get_or_insert((arrival_us, ext_ts));
        let d = (arrival_us as i64 - a0 as i64)
            - ((ext_ts - t0) as f64 / SAMPLES_PER_US).round() as i64;

        let prev_newest = self.newest_arrival_us;
        self.newest_arrival_us = self.newest_arrival_us.max(arrival_us);
        let horizon = self.newest_arrival_us.saturating_sub(BASELINE_WINDOW_US);
        while self.window.front().is_some_and(|&(a, _)| a < horizon) {
            self.window.pop_front();
        }
        while self.window.back().is_some_and(|&(_, bd)| bd >= d) {
            self.window.pop_back();
        }
        self.window.push_back((arrival_us, d));
        let min_d = self.window.front().map(|&(_, d)| d).unwrap_or(d);
        let rel = (d - min_d).max(0);

        let dt_us = self.newest_arrival_us.saturating_sub(prev_newest) as f64;
        let decayed = self.peak_us - DECAY_US_PER_S * dt_us / 1e6;
        self.peak_us = if rel as f64 > self.reject_us {
            decayed.max(0.0)
        } else {
            decayed.max((rel as f64).min(self.peak_cap_us))
        };
        self.last_rel_us = rel;
        rel
    }

    /// Target playout delay in milliseconds: peak relative delay plus the safety margin,
    /// clamped to the configured bounds.
    pub fn target_ms(&self) -> f64 {
        (self.peak_us / 1000.0 + self.margin_ms as f64)
            .clamp(self.min_delay_ms as f64, self.max_delay_ms as f64)
    }

    pub fn last_relative_delay_ms(&self) -> f64 {
        self.last_rel_us as f64 / 1000.0
    }

    pub fn peak_ms(&self) -> f64 {
        self.peak_us / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(est: &mut DelayEstimator, n: usize, start_ts: i64, extra: impl Fn(usize) -> u64) {
        for i in 0..n {
            let ts = start_ts + i as i64 * 960;
            est.update(ts as u64 * 1000 / 48 + extra(i), ts);
        }
    }

    #[test]
    fn clean_stream_sits_at_the_minimum() {
        let mut e = DelayEstimator::new(20, 250, 10);
        feed(&mut e, 200, 0, |_| 0);
        assert_eq!(e.target_ms(), 20.0);
    }

    #[test]
    fn a_delay_step_is_seen_at_once_and_then_decays() {
        let mut e = DelayEstimator::new(20, 250, 10);
        feed(&mut e, 100, 0, |_| 0);
        // Everything is 60 ms later from packet 100 on: the first late packet is a 60 ms spike.
        feed(&mut e, 1, 100 * 960, |_| 60_000);
        assert!(e.target_ms() >= 69.0, "target {}", e.target_ms());
        // After the 2 s baseline window the step is the new normal and the peak decays away.
        for k in 0..600 {
            let ts = (101 + k) as i64 * 960;
            e.update(ts as u64 * 1000 / 48 + 60_000, ts);
        }
        assert!(e.target_ms() < 45.0, "target {}", e.target_ms());
    }

    #[test]
    fn sparse_spikes_keep_the_target_up() {
        let mut e = DelayEstimator::new(20, 250, 10);
        // One 80 ms spike every 100 packets (2 s): invisible to a 97th-percentile estimate.
        let mut min_after_first = f64::MAX;
        for i in 0..1000usize {
            let ts = i as i64 * 960;
            let spike = if i % 100 == 50 { 80_000 } else { 0 };
            e.update(ts as u64 * 1000 / 48 + spike, ts);
            if i > 60 {
                min_after_first = min_after_first.min(e.target_ms());
            }
        }
        assert!(min_after_first >= 60.0, "target fell to {min_after_first}");
    }

    #[test]
    fn one_packet_0_8s_late_does_not_move_the_target() {
        let mut e = DelayEstimator::new(20, 250, 10);
        feed(&mut e, 100, 0, |_| 0);
        feed(&mut e, 1, 100 * 960, |_| 800_000);
        assert_eq!(e.target_ms(), 20.0, "0.8 s is beyond anything the buffer could cover");
    }

    #[test]
    fn a_late_packet_inside_the_range_decays_within_46_s() {
        let mut e = DelayEstimator::new(20, 250, 10);
        feed(&mut e, 100, 0, |_| 0);
        feed(&mut e, 1, 100 * 960, |_| 400_000);
        assert!(e.target_ms() <= 250.0 && e.peak_ms() <= 230.0, "peak {}", e.peak_ms());
        // 47 s of clean packets later (50 packets per second).
        for k in 0..(47 * 50) {
            let ts = (101 + k) as i64 * 960;
            e.update(ts as u64 * 1000 / 48, ts);
        }
        assert_eq!(e.target_ms(), 20.0, "target {} after 47 s", e.target_ms());
    }

    #[test]
    fn a_stray_timestamp_ten_minutes_ahead_does_not_blow_up_the_peak() {
        let mut e = DelayEstimator::new(20, 250, 10);
        feed(&mut e, 100, 0, |_| 0);
        // One packet whose timestamp is 10 min ahead, then the stream carries on.
        let stray = 100 * 960 + 10 * 60 * 48_000;
        e.update(100 * 20_000, stray);
        let mut worst: f64 = 0.0;
        for k in 101..(101 + 6 * 50) {
            let ts = k as i64 * 960;
            e.update(ts as u64 * 1000 / 48, ts);
            worst = worst.max(e.peak_ms());
        }
        assert!(worst <= 230.0, "peak reached {worst} ms");
        assert_eq!(e.target_ms(), 20.0, "recovered: {}", e.target_ms());
    }

    #[test]
    fn clock_drift_does_not_accumulate() {
        for ppm in [500i64, -500] {
            let mut e = DelayEstimator::new(20, 250, 10);
            for i in 0..3000usize {
                let ts = i as i64 * 960;
                let a = (ts as f64 / 0.048 * (1.0 + ppm as f64 * 1e-6)) as u64;
                e.update(a, ts);
            }
            assert!(e.target_ms() <= 22.0, "ppm {ppm}: target {}", e.target_ms());
        }
    }
}

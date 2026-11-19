//! Accelerate and Preemptive Expand time-stretching, used only for jitter-buffer level control.
//!
//! Copied from the NetEq-inspired `neteq` crate in hiinaspace's videocall-rs fork
//! (rev e854b5a, `neteq/src/time_stretch.rs` and the two helpers from `neteq/src/signal.rs`).
//! Original copyright 2025 Security Union LLC, licensed under either of Apache-2.0 or MIT at
//! your option; this copy keeps that dual licence. Changes: the helpers are inlined here,
//! the `unsafe impl Send` lines are dropped (the types are plain data, so `Send` is automatic),
//! and the tests are rewritten for this crate.

/// Compute the best normalized cross-correlation of given length
/// between two equal-length slices.
pub fn best_normalized_correlation(a: &[f32], b: &[f32], len: usize) -> (usize, f32) {
    debug_assert_eq!(a.len(), b.len());
    debug_assert!(a.len() >= len);

    let mut corr = 0.0;
    let mut energy = 0.0;
    let mut best_pos = 0;
    let mut best_corr = -1.0;
    for i in 0..a.len() {
        let x = a[i];
        let y = b[i];
        corr += x * y;
        energy += (x * x).max(y * y);
        if i + 1 >= len {
            let normalized_corr = if energy == 0.0 { 1.0 } else { corr / energy };

            let i_start = i + 1 - len;

            if normalized_corr >= 1.0 {
                return (i_start, 1.0);
            }

            if normalized_corr > best_corr {
                best_corr = normalized_corr;
                best_pos = i_start;
            }

            let old_x = a[i_start];
            let old_y = b[i_start];
            corr -= old_x * old_y;
            energy -= (old_x * old_x).max(old_y * old_y);
        }
    }

    (best_pos, best_corr)
}

/// Cross-fade `fade_len` samples between the tail of `prev` and the head of `next`,
/// writing into `out`. `prev` and `next` must both have at least `fade_len` samples.
pub fn crossfade(prev: &[f32], next: &[f32], fade_len: usize, out: &mut [f32]) {
    let fade_start = prev.len() - fade_len;
    // copy first part (prev without last fade_len)
    out[..fade_start].copy_from_slice(&prev[..fade_start]);

    for i in 0..fade_len {
        let fade_out = 1.0 - (i as f32 / fade_len as f32);
        let fade_in = 1.0 - fade_out;
        let sample = prev[fade_start + i] * fade_out + next[i] * fade_in;
        out[fade_start + i] = sample;
    }
}

const ENERGY_THRESHOLD_DECREASE_FACTOR: f32 = 0.995;
const ENERGY_THRESHOLD_INCREASE_FACTOR: f32 = 1.002;

/// Return codes for time-stretching operations
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeStretchResult {
    Success,
    SuccessLowEnergy,
    NoStretch,
    Error,
}

/// Audio time-stretching processor
pub trait TimeStretcher {
    /// Process audio samples and return the result
    fn process(&mut self, input: &[f32], output: &mut [f32], fast_mode: bool) -> TimeStretchResult;

    // Returns the number of input samples used in the last operation
    fn get_used_input_samples(&self) -> usize;

    /// Reset the time stretcher state
    fn reset(&mut self);
}

/// Accelerate algorithm - removes audio samples to speed up playback
#[derive(Debug)]
pub struct Accelerate {
    _sample_rate: u32,
    _channels: u8,
    used_input_samples: usize,
    overlap_length: usize,
    _max_change_rate: f32,
    low_energy_threshold: f32,
}

impl Accelerate {
    /// Create a new accelerate processor
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            _sample_rate: sample_rate,
            _channels: channels,
            used_input_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            _max_change_rate: 0.25, // Maximum 25% reduction
            low_energy_threshold: 0.0001,
        }
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.003) as usize).max(32) // Minimum 32 samples
    }

    /// Accelerate the audio by removing samples
    fn accelerate_internal(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        fast_mode: bool,
    ) -> TimeStretchResult {
        if input.len() <= output.len() {
            // Input should always be longer then output for accelerate
            output[..input.len()].copy_from_slice(input);
            self.used_input_samples = input.len();
            return TimeStretchResult::NoStretch;
        }

        if output.len() < self.overlap_length * 2 {
            // Not enough samples to accelerate
            output.copy_from_slice(&input[..output.len()]);
            self.used_input_samples = output.len();
            return TimeStretchResult::NoStretch;
        }

        let max_remove_low_energy = if fast_mode {
            self.max_samples_to_remove(input, output, 0.4)
        } else {
            self.max_samples_to_remove(input, output, 0.2)
        };
        let max_remove_high_energy = if fast_mode {
            self.max_samples_to_remove(input, output, 0.25)
        } else {
            // Don't use high energy remove in normal mode
            0
        };

        let (mut best_pos, mut samples_to_remove) =
            self.find_low_energy_to_remove(input, output, max_remove_low_energy);

        let mut is_low_energy = true;
        if !fast_mode {
            if samples_to_remove == 0 {
                // If no low-energy zone found, don't remove anything in normal mode
                output.copy_from_slice(&input[..output.len()]);
                self.used_input_samples = output.len();
                return TimeStretchResult::NoStretch;
            }
        } else if samples_to_remove < max_remove_high_energy {
            // In fast mode, always remove at least max_remove_high_energy samples
            is_low_energy = false;
            samples_to_remove = max_remove_high_energy;

            // Find the best location to remove samples (lowest energy region)
            best_pos = self.find_best_removal_point(
                &input[..output.len() + samples_to_remove],
                samples_to_remove,
            );
        }

        let usable_input = &input[..output.len() + samples_to_remove];

        // original samples: 12346789
        // samples_to_remove = 4, overlap_length = 2
        // original:     |123456789|
        // head+overlap: |1234 | // head_length = 2
        // overlap+tail: |  789| // tail_start = 6
        // result:       |12**9| // 2 samples overlapped
        // head_length+overlap_length+(usable_input.len()-tail_start-overlap_length) = output.len()
        // head_length+usable_input.len()-tail_start = output.len()
        // tail_start= head_length+usable_input.len()-output.len() = head_length+(output.len()+samples_to_remove)-output.len() = head_length+samples_to_remove

        // Copy the first part
        output[..best_pos].copy_from_slice(&usable_input[..best_pos]);

        crossfade(
            &usable_input[best_pos..best_pos + self.overlap_length],
            &usable_input
                [best_pos + samples_to_remove..best_pos + samples_to_remove + self.overlap_length],
            self.overlap_length,
            &mut output[best_pos..best_pos + self.overlap_length],
        );

        // Copy the rest
        output[best_pos + self.overlap_length..]
            .copy_from_slice(&usable_input[best_pos + samples_to_remove + self.overlap_length..]);

        self.used_input_samples = usable_input.len();

        if is_low_energy {
            TimeStretchResult::SuccessLowEnergy
        } else {
            TimeStretchResult::Success
        }
    }

    fn max_samples_to_remove(
        &mut self,
        input: &[f32],
        output: &[f32],
        acceleration_factor: f32,
    ) -> usize {
        let max_remove = (output.len() as f32 * acceleration_factor) as usize;
        let max_remove = max_remove.min(output.len() / 2); // Don't remove more than 1/3 of input (= 1/2 of output)
        let max_remove = max_remove.min(input.len() - output.len());
        max_remove.max(self.overlap_length)
    }

    fn find_low_energy_to_remove(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        max_remove: usize,
    ) -> (usize, usize) {
        let usable_input = &input[..output.len() + max_remove];

        let (best_pos, best_len) = Self::longest_low_energy_region(
            usable_input,
            self.low_energy_threshold,
            |i: usize, len: usize| -> bool {
                i + len.saturating_sub(self.overlap_length).min(max_remove) <= output.len()
            },
        );

        if best_len > max_remove {
            // we have more than enough to remove, lower the energy threshold
            self.low_energy_threshold *= ENERGY_THRESHOLD_DECREASE_FACTOR;
        } else if best_len < max_remove / 2 {
            // not enough to remove, slowly increase the energy threshold
            self.low_energy_threshold *= ENERGY_THRESHOLD_INCREASE_FACTOR;
        }

        let samples_to_remove = best_len.saturating_sub(self.overlap_length).min(max_remove);

        if samples_to_remove < self.overlap_length {
            return (0, 0);
        }

        (best_pos, samples_to_remove)
    }

    fn calculate_energy(&self, samples: &[f32]) -> f32 {
        let sum_squares: f32 = samples.iter().map(|x| x * x).sum();
        sum_squares / samples.len() as f32
    }

    fn find_best_removal_point(&self, input: &[f32], removal_length: usize) -> usize {
        let mut best_position = input.len() / 3; // Default to middle-ish
        let mut lowest_energy = f32::MAX;

        // Search for the lowest energy region to remove
        let search_start = self.overlap_length;
        let search_end = input
            .len()
            .saturating_sub(removal_length + self.overlap_length);

        for pos in (search_start..search_end).step_by(self.overlap_length / 2) {
            let end_pos = (pos + removal_length).min(input.len());
            let region_energy = self.calculate_energy(&input[pos..end_pos]);

            if region_energy < lowest_energy {
                lowest_energy = region_energy;
                best_position = pos;
            }
        }

        best_position
    }

    fn longest_low_energy_region<F>(
        input: &[f32],
        threshold: f32,
        validate_candidate: F,
    ) -> (usize, usize)
    where
        F: Fn(usize, usize) -> bool, // closure type
    {
        // Step 1: define energy as x^2. Compute the rolling sum of the energy deviation from
        // threshold. In this rolling sum, if rolling_sum[j] - rolling_sum[j] <= 0, that means
        // that the sum of the deviation from threshold between i and j is less then 0,
        // which means that AVG(a[i..j]) <= threshold
        // This algorithm finds the the largest range where that stands. The constainst
        // validate_candidate is used to make sure that the final output size is correct.
        let mut energy_deviation_rolling_sum = Vec::with_capacity(input.len() + 1);
        energy_deviation_rolling_sum.push(0.0);
        for &x in input {
            energy_deviation_rolling_sum
                .push(energy_deviation_rolling_sum.last().unwrap() + x * x - threshold);
        }

        // Step 2: build increasing peaks stack
        // Rationale: if the range [i..j] statisfies the condition that rolling_sum[j] - rolling_sum[j] <= 0
        // then any i' < i, where rolling_sum[i'] > rolling_sum[i] will also statisfy the condition.
        // In other words, we need to find candidates that do not have a previous higher value.
        let mut stack = Vec::new(); // stores indices of rolling_sum
        for (i, &val) in energy_deviation_rolling_sum.iter().enumerate() {
            if stack.is_empty() || val > energy_deviation_rolling_sum[*stack.last().unwrap()] {
                stack.push(i);
            }
        }

        // Step 3: reverse scan to find longest valid subarray
        let mut max_len = 0;
        let mut best_i = 0;

        for j in (0..energy_deviation_rolling_sum.len()).rev() {
            while !stack.is_empty() && stack[stack.len() - 1] >= j {
                stack.pop();
            }
            if stack.is_empty() {
                break;
            }
            let mut si = stack.len();
            while si > 0 {
                si -= 1;
                let i = stack[si];
                // Find the smallest i where the condition holds
                if energy_deviation_rolling_sum[j] <= energy_deviation_rolling_sum[i] {
                    let len = j - i;
                    if len > max_len && validate_candidate(i, len) {
                        max_len = len;
                        best_i = i;
                        stack.truncate(si);
                    }
                } else {
                    break;
                }
            }
        }

        (best_i, max_len)
    }
}

impl TimeStretcher for Accelerate {
    fn process(&mut self, input: &[f32], output: &mut [f32], fast_mode: bool) -> TimeStretchResult {
        self.accelerate_internal(input, output, fast_mode)
    }

    fn get_used_input_samples(&self) -> usize {
        self.used_input_samples
    }

    fn reset(&mut self) {
        self.used_input_samples = 0;
    }
}

/// Preemptive Expand algorithm - adds audio samples to slow down playback
#[derive(Debug)]
pub struct PreemptiveExpand {
    _sample_rate: u32,
    _channels: u8,
    used_input_samples: usize,
    overlap_length: usize,
    corr_threshold: f32,
}

impl PreemptiveExpand {
    /// Create a new preemptive expand processor
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            _sample_rate: sample_rate,
            _channels: channels,
            used_input_samples: 0,
            overlap_length: Self::calculate_overlap_length(sample_rate),
            corr_threshold: 0.99,
        }
    }

    fn calculate_overlap_length(sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.003) as usize).max(32) // Minimum 32 samples
    }

    /// Expand the audio by duplicating/stretching samples
    fn expand_internal(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        _fast_mode: bool,
    ) -> TimeStretchResult {
        if input.len() < output.len() {
            // Input should always be at least the length of output
            output[..input.len()].copy_from_slice(input);
            self.used_input_samples = input.len();
            return TimeStretchResult::NoStretch;
        }

        // Calculate energy of the input signal
        let energy = self.calculate_energy(input);
        let is_low_energy = energy < 0.01; // Threshold for low energy detection

        // With 30ms input overlap_length is 3ms and max_add is about 13.5ms. Human voice
        // is around 85 to 400Hz (including children). 85Hz is about 12ms, 400Hz is about
        // 2.5ms. The correlation lag (aka: add_len) goes from overlap_length (3ms) to
        // max_add (13.5ms), which mostly covers the range of the human voice.
        let max_add = (output.len() - self.overlap_length) / 2;
        if max_add < self.overlap_length {
            // Not enough samples to expand
            output.copy_from_slice(&input[..output.len()]);
            self.used_input_samples = output.len();
            return TimeStretchResult::NoStretch;
        }

        let mut best_corr = -1.0f32;
        let mut best_pos = 0;
        let mut best_add_len = 0;
        for add_len in self.overlap_length..max_add {
            let correlation_len = output.len() - add_len * 2;
            let (pos, corr) = best_normalized_correlation(
                &input[add_len..correlation_len + add_len],
                &input[..correlation_len],
                self.overlap_length,
            );
            if corr > best_corr {
                best_corr = corr;
                best_pos = pos;
                best_add_len = add_len;
            }
        }

        if best_corr < self.corr_threshold {
            self.corr_threshold *= 0.98;

            // Correlation is too low
            output.copy_from_slice(&input[..output.len()]);
            self.used_input_samples = output.len();
            return TimeStretchResult::NoStretch;
        } else {
            self.corr_threshold = self.corr_threshold.max(best_corr * 0.95);
        }

        let usable_input = &input[..output.len() - best_add_len];

        // Copy first part
        output[..best_add_len + best_pos].copy_from_slice(&usable_input[..best_add_len + best_pos]);

        // Crossfade duplicate region
        crossfade(
            &usable_input[best_add_len + best_pos..best_add_len + best_pos + self.overlap_length],
            &usable_input[best_pos..best_pos + self.overlap_length],
            self.overlap_length,
            &mut output[best_add_len + best_pos..best_add_len + best_pos + self.overlap_length],
        );

        // Append rest
        output[best_add_len + best_pos + self.overlap_length..]
            .copy_from_slice(&usable_input[best_pos + self.overlap_length..]);

        self.used_input_samples = usable_input.len();

        if is_low_energy {
            TimeStretchResult::SuccessLowEnergy
        } else {
            TimeStretchResult::Success
        }
    }

    fn calculate_energy(&self, samples: &[f32]) -> f32 {
        let sum_squares: f32 = samples.iter().map(|x| x * x).sum();
        sum_squares / samples.len() as f32
    }
}

impl TimeStretcher for PreemptiveExpand {
    fn process(&mut self, input: &[f32], output: &mut [f32], fast_mode: bool) -> TimeStretchResult {
        self.expand_internal(input, output, fast_mode)
    }

    fn get_used_input_samples(&self) -> usize {
        self.used_input_samples
    }

    fn reset(&mut self) {
        self.used_input_samples = 0;
    }
}

/// Time stretch factory for creating time stretchers
pub struct TimeStretchFactory;

impl TimeStretchFactory {
    /// Create an accelerate processor
    pub fn create_accelerate(sample_rate: u32, channels: u8) -> Box<dyn TimeStretcher + Send> {
        Box::new(Accelerate::new(sample_rate, channels))
    }

    /// Create a preemptive expand processor  
    pub fn create_preemptive_expand(
        sample_rate: u32,
        channels: u8,
    ) -> Box<dyn TimeStretcher + Send> {
        Box::new(PreemptiveExpand::new(sample_rate, channels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voiced(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                0.3 * ((2.0 * std::f32::consts::PI * 140.0 * t).sin()
                    + 0.5 * (2.0 * std::f32::consts::PI * 280.0 * t).sin()
                    + 0.25 * (2.0 * std::f32::consts::PI * 420.0 * t).sin())
            })
            .collect()
    }

    fn max_step(x: &[f32]) -> f32 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    /// Accelerate in normal mode only cuts inside a low-energy stretch (between words), so give
    /// it one.
    #[test]
    fn accelerate_cuts_in_a_quiet_stretch_without_a_click() {
        let mut input = voiced(1440);
        for v in &mut input[400..1000] {
            *v *= 0.002;
        }
        let mut acc = Accelerate::new(48_000, 1);
        let mut out = vec![0.0; 960];
        let res = acc.process(&input, &mut out, false);
        assert!(!matches!(res, TimeStretchResult::NoStretch));
        let used = acc.get_used_input_samples();
        assert!(used > 960 && used <= 1440, "used {used}");
        assert!(max_step(&out) <= max_step(&input) * 1.3);
    }

    #[test]
    fn accelerate_leaves_steady_voiced_audio_alone_in_normal_mode() {
        let input = voiced(1440);
        let mut acc = Accelerate::new(48_000, 1);
        let mut out = vec![0.0; 960];
        let res = acc.process(&input, &mut out, false);
        assert!(matches!(res, TimeStretchResult::NoStretch));
        assert_eq!(acc.get_used_input_samples(), 960);
        assert_eq!(&out[..], &input[..960]);
    }

    #[test]
    fn preemptive_expand_adds_a_pitch_period_without_a_click() {
        let input = voiced(1440);
        let mut pe = PreemptiveExpand::new(48_000, 1);
        let mut out = vec![0.0; 1440];
        let res = pe.process(&input, &mut out, false);
        assert!(!matches!(res, TimeStretchResult::NoStretch));
        let used = pe.get_used_input_samples();
        assert!(used < 1440 && used >= 960, "used {used}");
        assert!(max_step(&out) <= max_step(&input) * 1.3);
    }

    /// CPU time this thread has used, in microseconds. Wall-clock time would make the
    /// assertion below a measurement of the host's load, not of the search.
    fn thread_cpu_us() -> f64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid out-pointer; CLOCK_THREAD_CPUTIME_ID exists on Linux.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        ts.tv_sec as f64 * 1e6 + ts.tv_nsec as f64 / 1e3
    }

    /// Cost of the correlation search at 48 kHz, to decide whether it needs downsampling.
    /// It failed once under a load average of ~11 with a wall-clock 5 ms bound in a debug
    /// build: the bound was on the scheduler as much as on the code. CPU time of this
    /// thread does not move with load; the bound is 5 ms per op for an optimised build and
    /// 10x that for an unoptimised one (the search is ~330 us optimised).
    #[test]
    fn search_cost_is_small() {
        let input = voiced(1440);
        let mut pe = PreemptiveExpand::new(48_000, 1);
        let mut acc = Accelerate::new(48_000, 1);
        let mut out = vec![0.0; 1440];
        let n = 200;
        let t = thread_cpu_us();
        for _ in 0..n {
            pe.process(&input, &mut out, false);
            acc.process(&input, &mut out[..960], false);
        }
        let per = (thread_cpu_us() - t) / (2.0 * n as f64);
        println!("time-stretch search: {per:.0} us of CPU per op");
        // A 20 ms block budget is 20,000 us; a handful of ops per second per peer.
        let bound = if cfg!(debug_assertions) { 50_000.0 } else { 5_000.0 };
        assert!(per < bound, "{per} us of CPU per op (bound {bound})");
    }
}

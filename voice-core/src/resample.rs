//! Streaming windowed-sinc resampler for the microphone path (any input rate to the 48 kHz
//! codec rate).
//!
//! The kernel is a Kaiser-windowed sinc whose cutoff sits just below the lower of the two
//! Nyquist frequencies, so downsampling is anti-aliased and upsampling leaves no images. The
//! output sample `n` depends only on input samples around `n * in_rate / out_rate` (indices
//! before the start of the stream read as zero), so the output is identical however the input
//! is split into chunks. The output lags the input by `kernel half-width` input samples.

use crate::error::{Error, Result};

/// Input rates outside this range are refused at construction.
pub const MIN_RATE: u32 = 8_000;
pub const MAX_RATE: u32 = 192_000;

const PHASES: usize = 512;
/// Kernel half-width, in samples of the *lower* of the two rates.
const HALF_TAPS: f64 = 40.0;
const KAISER_BETA: f64 = 9.0;
/// Cutoff as a fraction of the lower Nyquist frequency.
const CUTOFF: f64 = 0.94;

#[derive(Debug, Clone)]
pub struct InputResampler {
    /// Reduced ratio: output sample `n` sits at input position `n * step_num / step_den`.
    step_num: u64,
    step_den: u64,
    passthrough: bool,
    half: usize,
    /// `PHASES + 1` rows of `2 * half` taps; row `p` is the kernel for fractional offset
    /// `p / PHASES`.
    table: Vec<f32>,
    /// Input samples, starting at absolute index `buf_start`.
    buf: Vec<f32>,
    buf_start: u64,
    /// Output samples produced so far.
    out_index: u64,
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < 1e-14 * sum {
            break;
        }
    }
    sum
}

impl InputResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self> {
        if !(MIN_RATE..=MAX_RATE).contains(&input_rate)
            || !(MIN_RATE..=MAX_RATE).contains(&output_rate)
        {
            return Err(Error::UnsupportedConfig(
                "sample rates must be within 8 kHz..192 kHz",
            ));
        }
        let g = gcd(input_rate as u64, output_rate as u64);
        let step_num = input_rate as u64 / g;
        let step_den = output_rate as u64 / g;
        let passthrough = input_rate == output_rate;

        // `scale` < 1 when downsampling: the kernel stretches in input samples.
        let scale = (output_rate as f64 / input_rate as f64).min(1.0);
        let half = (HALF_TAPS / scale).ceil() as usize;
        let fc = 0.5 * scale * CUTOFF; // cycles per input sample
        let i0_beta = bessel_i0(KAISER_BETA);
        let mut table = Vec::new();
        if !passthrough {
            table = vec![0.0f32; (PHASES + 1) * 2 * half];
            for p in 0..=PHASES {
                let frac = p as f64 / PHASES as f64;
                let mut row_sum = 0.0;
                let row = &mut table[p * 2 * half..(p + 1) * 2 * half];
                for (k, tap) in row.iter_mut().enumerate() {
                    // Tap k reads input index i0 - (half - 1) + k, at offset x from the
                    // output position i0 + frac.
                    let x = (k as f64 - (half as f64 - 1.0)) - frac;
                    let sinc = if x.abs() < 1e-12 {
                        2.0 * fc
                    } else {
                        (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
                    };
                    let r = x / half as f64;
                    let w = if r.abs() >= 1.0 {
                        0.0
                    } else {
                        bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta
                    };
                    let v = sinc * w;
                    row_sum += v;
                    *tap = v as f32;
                }
                // Unity gain at DC for every phase.
                for tap in row.iter_mut() {
                    *tap = (*tap as f64 / row_sum) as f32;
                }
            }
        }

        Ok(Self {
            step_num,
            step_den,
            passthrough,
            half,
            table,
            buf: Vec::new(),
            buf_start: 0,
            out_index: 0,
        })
    }

    /// Samples of input delay (the kernel half-width); 0 for a pass-through.
    pub fn latency_input_samples(&self) -> usize {
        if self.passthrough {
            0
        } else {
            self.half
        }
    }

    /// Append the resampled version of `input` to `out`. Any chunk length, including 0, is fine.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let buf_end = self.buf_start + self.buf.len() as u64;
        let half = self.half as u64;
        let taps = 2 * self.half;
        loop {
            let pos_num = self.out_index * self.step_num;
            let i0 = pos_num / self.step_den;
            // Taps read input indices i0 - (half - 1) ..= i0 + half.
            if i0 + half >= buf_end {
                break;
            }
            let frac_num = pos_num % self.step_den;
            let phase_f = frac_num as f64 * PHASES as f64 / self.step_den as f64;
            let p0 = phase_f.floor() as usize;
            let mix = (phase_f - p0 as f64) as f32;
            let row0 = &self.table[p0 * taps..(p0 + 1) * taps];
            let row1 = &self.table[(p0 + 1) * taps..(p0 + 2) * taps];
            let first = i0 as i64 - (half as i64 - 1);
            let mut acc0 = 0.0f32;
            let mut acc1 = 0.0f32;
            for k in 0..taps {
                let idx = first + k as i64;
                if idx < 0 {
                    continue; // zero before the start of the stream
                }
                let s = self.buf[(idx as u64 - self.buf_start) as usize];
                acc0 += s * row0[k];
                acc1 += s * row1[k];
            }
            out.push(acc0 + (acc1 - acc0) * mix);
            self.out_index += 1;
        }
        // Drop input no later output can read.
        let next_first =
            ((self.out_index * self.step_num) / self.step_den) as i64 - (half as i64 - 1);
        if next_first > self.buf_start as i64 {
            let drop = (next_first as u64 - self.buf_start) as usize;
            let drop = drop.min(self.buf.len());
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn run(inr: u32, outr: u32, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut r = InputResampler::new(inr, outr).unwrap();
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            r.process(c, &mut out);
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn pass_through_preserves_samples() {
        let mut resampler = InputResampler::new(48_000, 48_000).unwrap();
        let mut out = Vec::new();
        resampler.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    /// Finding 9: 192 kHz input in 3-sample chunks used to panic.
    #[test]
    fn small_chunks_at_192k_do_not_panic_and_count_is_right() {
        let input = tone(192_000, 1_000.0, 192_000, 0.5);
        let out = run(192_000, 48_000, &input, 3);
        // One second of input: 48000 output samples, less the unproduced kernel tail.
        let r = InputResampler::new(192_000, 48_000).unwrap();
        let lag_out = (r.latency_input_samples() / 4) + 1;
        assert!(
            (out.len() as i64 - 48_000).unsigned_abs() as usize <= lag_out,
            "got {} samples",
            out.len()
        );
    }

    #[test]
    fn output_is_identical_for_any_chunking() {
        for (inr, outr) in [(44_100, 48_000), (96_000, 48_000), (16_000, 48_000), (192_000, 48_000), (8_000, 48_000), (48_000, 44_100)] {
            let input = tone(inr, 1_234.5, inr as usize / 2, 0.4);
            let whole = run(inr, outr, &input, input.len());
            for chunk in [1usize, 3, 7, 100, 441, 1023] {
                let parts = run(inr, outr, &input, chunk);
                assert_eq!(whole.len(), parts.len(), "{inr}->{outr} chunk {chunk}");
                assert_eq!(whole, parts, "{inr}->{outr} chunk {chunk}");
            }
        }
    }

    #[test]
    fn output_count_matches_the_rate_ratio() {
        for (inr, outr) in [(44_100u32, 48_000u32), (16_000, 48_000), (96_000, 48_000), (11_025, 48_000), (22_050, 48_000), (192_000, 48_000), (47_999, 48_000)] {
            let n = inr as usize; // one second
            let out = run(inr, outr, &vec![0.1; n], 480);
            let r = InputResampler::new(inr, outr).unwrap();
            let slack = (r.latency_input_samples() as f64 * outr as f64 / inr as f64).ceil() as i64 + 2;
            assert!(
                (out.len() as i64 - outr as i64).abs() <= slack,
                "{inr}->{outr}: {} vs {outr}",
                out.len()
            );
        }
    }

    /// Finding 9: no anti-aliasing. A 30 kHz tone at 96 kHz folds to 18 kHz at 48 kHz unless
    /// the resampler filters it.
    #[test]
    fn alias_of_a_30khz_tone_at_96k_is_rejected() {
        let input = tone(96_000, 30_000.0, 96_000, 0.5);
        let out = run(96_000, 48_000, &input, 480);
        let settled = &out[2_000..out.len() - 2_000];
        let level_db = 20.0 * (rms(settled) / (0.5 / 2f32.sqrt())).log10();
        assert!(level_db < -60.0, "alias at {level_db:.1} dB re the input");
        // The old linear interpolator passes it at about -3 dB.
    }

    #[test]
    fn passband_is_flat_and_upsampling_leaves_no_images() {
        let input = tone(16_000, 3_000.0, 16_000, 0.5);
        let out = run(16_000, 48_000, &input, 160);
        let settled = &out[2_000..out.len() - 2_000];
        let gain_db = 20.0 * (rms(settled) / (0.5 / 2f32.sqrt())).log10();
        assert!(gain_db.abs() < 0.1, "passband gain {gain_db:.3} dB");
        // Image at 16k - 3k = 13 kHz: project onto it.
        let (mut c, mut s) = (0.0f64, 0.0f64);
        for (i, v) in settled.iter().enumerate() {
            let ph = 2.0 * std::f64::consts::PI * 13_000.0 * (i + 2_000) as f64 / 48_000.0;
            c += *v as f64 * ph.cos();
            s += *v as f64 * ph.sin();
        }
        let amp = 2.0 * (c * c + s * s).sqrt() / settled.len() as f64;
        assert!(20.0 * (amp / 0.5).log10() < -60.0, "image amplitude {amp}");
    }

    #[test]
    fn rejects_rates_outside_the_supported_range() {
        assert!(InputResampler::new(0, 48_000).is_err());
        assert!(InputResampler::new(7_999, 48_000).is_err());
        assert!(InputResampler::new(192_001, 48_000).is_err());
        assert!(InputResampler::new(8_000, 48_000).is_ok());
        assert!(InputResampler::new(192_000, 48_000).is_ok());
    }
}

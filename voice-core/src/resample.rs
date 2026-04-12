use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct InputResampler {
    input_rate: u32,
    output_rate: u32,
    source_pos: f64,
    history: Vec<f32>,
}

impl InputResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self> {
        if input_rate == 0 || output_rate == 0 {
            return Err(Error::UnsupportedConfig("sample rates must be positive"));
        }

        Ok(Self {
            input_rate,
            output_rate,
            source_pos: 0.0,
            history: Vec::new(),
        })
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }

        if self.input_rate == self.output_rate {
            out.extend_from_slice(input);
            return;
        }

        self.history.extend_from_slice(input);
        let step = self.input_rate as f64 / self.output_rate as f64;

        while self.source_pos + 1.0 < self.history.len() as f64 {
            let idx = self.source_pos.floor() as usize;
            let frac = (self.source_pos - idx as f64) as f32;
            let a = self.history[idx];
            let b = self.history[idx + 1];
            out.push(a + (b - a) * frac);
            self.source_pos += step;
        }

        let keep_from = self.source_pos.floor() as usize;
        if keep_from > 0 {
            self.history.drain(..keep_from);
            self.source_pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_through_preserves_samples() {
        let mut resampler = InputResampler::new(48_000, 48_000).unwrap();
        let mut out = Vec::new();
        resampler.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn resamples_441_to_480() {
        let mut resampler = InputResampler::new(44_100, 48_000).unwrap();
        let input: Vec<f32> = (0..441).map(|i| i as f32 / 441.0).collect();
        let mut out = Vec::new();
        resampler.process(&input, &mut out);
        assert!(!out.is_empty());
        assert!((out.len() as i32 - 480).abs() <= 1);
    }
}

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct InputResampler {
    input_rate: u32,
    output_rate: u32,
}

impl InputResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self> {
        if input_rate != output_rate {
            return Err(Error::UnsupportedConfig(
                "milestone 1 only supports pass-through resampling",
            ));
        }
        Ok(Self {
            input_rate,
            output_rate,
        })
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let _ = (self.input_rate, self.output_rate);
        out.extend_from_slice(input);
    }
}

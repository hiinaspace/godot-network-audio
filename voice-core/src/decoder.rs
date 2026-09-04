use std::convert::TryInto;

use audiopus::coder::{Decoder as OpusDecoder, GenericCtl};
use audiopus::{Channels, SampleRate};

use crate::error::{Error, Result};

pub struct OpusAudioDecoder {
    inner: OpusDecoder,
    sample_rate: u32,
    channels: u8,
}

impl OpusAudioDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let sample_rate = match sample_rate {
            8_000 => SampleRate::Hz8000,
            12_000 => SampleRate::Hz12000,
            16_000 => SampleRate::Hz16000,
            24_000 => SampleRate::Hz24000,
            48_000 => SampleRate::Hz48000,
            _ => return Err(Error::UnsupportedConfig("unsupported opus sample rate")),
        };
        let channels = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => return Err(Error::UnsupportedConfig("unsupported opus channel count")),
        };

        let inner = OpusDecoder::new(sample_rate, channels)
            .map_err(|e| Error::Opus(format!("decoder init: {e}")))?;

        Ok(Self {
            inner,
            sample_rate: match sample_rate {
                SampleRate::Hz8000 => 8_000,
                SampleRate::Hz12000 => 12_000,
                SampleRate::Hz16000 => 16_000,
                SampleRate::Hz24000 => 24_000,
                SampleRate::Hz48000 => 48_000,
            },
            channels: match channels {
                Channels::Mono => 1,
                Channels::Stereo => 2,
                Channels::Auto => unreachable!("auto channel count is not valid here"),
            },
        })
    }
}

impl neteq::codec::AudioDecoder for OpusAudioDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> neteq::Result<Vec<f32>> {
        let max_samples_per_channel = (self.sample_rate as usize * 120) / 1000;
        let mut out = vec![0.0f32; max_samples_per_channel * self.channels as usize];
        let decoded = self
            .inner
            .decode_float(
                Some(encoded.try_into().map_err(|e| {
                    neteq::NetEqError::DecoderError(format!("invalid opus packet: {e}"))
                })?),
                (&mut out).try_into().map_err(|e| {
                    neteq::NetEqError::DecoderError(format!("invalid output buffer: {e}"))
                })?,
                false,
            )
            .map_err(|e| neteq::NetEqError::DecoderError(format!("Opus decode: {e}")))?;
        out.truncate(decoded * self.channels as usize);
        Ok(out)
    }

    fn reset(&mut self) -> neteq::Result<()> {
        self.inner
            .reset_state()
            .map_err(|e| neteq::NetEqError::DecoderError(format!("Opus reset: {e}")))
    }
}

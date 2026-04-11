use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use voice_core::fixture_harness::{
    default_profile, linear_resample_mono, rms, run_impairment_harness, SAMPLE_RATE,
};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let input_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test44100.wav"));
    let output_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/fixture_harness_output.wav"));

    let (input_samples, input_rate) = read_wav_mono_f32(&input_path)?;
    let input_48k = linear_resample_mono(&input_samples, input_rate, SAMPLE_RATE);
    let profile = default_profile();
    let run = run_impairment_harness(&input_48k, &profile)?;

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    write_wav_mono_f32(&output_path, &run.output_samples, SAMPLE_RATE)?;

    println!("input: {}", input_path.display());
    println!("output: {}", output_path.display());
    println!("profile: {}", profile.name);
    println!("input_rate_hz: {input_rate}");
    println!("resampled_input_samples: {}", run.input_samples.len());
    println!("output_samples: {}", run.output_samples.len());
    println!("input_rms: {:.6}", rms(&run.input_samples));
    println!("output_rms: {:.6}", rms(&run.output_samples));
    println!(
        "measured_output_rms: {:.6}",
        run.metrics.measured_output_rms
    );
    println!(
        "avg_preferred_buffer_size_ms: {:.2}",
        run.metrics.avg_preferred_buffer_size_ms
    );
    println!(
        "avg_target_delay_ms: {:.2}",
        run.metrics.avg_target_delay_ms
    );
    println!(
        "avg_current_buffer_size_ms: {:.2}",
        run.metrics.avg_current_buffer_size_ms
    );
    println!(
        "avg_expand_rate_q14: {:.2}",
        run.metrics.avg_expand_rate_q14
    );
    println!(
        "avg_accelerate_rate_q14: {:.2}",
        run.metrics.avg_accelerate_rate_q14
    );
    println!(
        "concealed_samples_delta: {}",
        run.metrics.concealed_samples_delta
    );
    println!("measured_frames: {}", run.metrics.measured_frames);
    println!(
        "final_packets_awaiting_decode: {}",
        run.final_stats.packets_awaiting_decode
    );
    println!(
        "consecutive_failures: {}",
        run.final_stats.consecutive_failures
    );
    println!(
        "sticky_error: {}",
        run.final_stats.sticky_error.as_deref().unwrap_or("<none>")
    );

    Ok(())
}

fn read_wav_mono_f32(path: &PathBuf) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        WavReader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels != 1 {
        bail!("expected mono wav, got {} channels", spec.channels);
    }

    let samples = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|sample| sample.map(|s| s as f32 / i16::MAX as f32))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read 16-bit PCM samples")?,
        (SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read 32-bit float samples")?,
        _ => bail!(
            "unsupported wav format: {:?} {} bits",
            spec.sample_format,
            spec.bits_per_sample
        ),
    };

    Ok((samples, spec.sample_rate))
}

fn write_wav_mono_f32(path: &PathBuf, samples: &[f32], sample_rate: u32) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(path, spec)
        .with_context(|| format!("failed to create {}", path.display()))?;
    for &sample in samples {
        writer
            .write_sample(sample.clamp(-1.0, 1.0))
            .with_context(|| format!("failed writing {}", path.display()))?;
    }
    writer
        .finalize()
        .with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(())
}

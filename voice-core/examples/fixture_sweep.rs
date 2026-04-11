use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use hound::{SampleFormat, WavReader};
use voice_core::fixture_harness::{
    linear_resample_mono, rms, run_impairment_harness_with_delay_bounds, sweep_profiles,
    HarnessOutput, ImpairmentProfile, SAMPLE_RATE,
};

#[derive(Debug)]
struct SweepRow {
    profile_name: &'static str,
    max_delay_ms: u32,
    input_rms: f32,
    output_rms: f32,
    rms_ratio: f32,
    preferred_buffer_size_ms: u32,
    target_delay_ms: u32,
    current_buffer_size_ms: u32,
    expand_rate_q14: u16,
    accelerate_rate_q14: u16,
    concealed_samples: u64,
    packets_awaiting_decode: usize,
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let input_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test44100.wav"));
    let csv_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/fixture_sweep.csv"));

    let (input_samples, input_rate) = read_wav_mono_f32(&input_path)?;
    let input_48k = linear_resample_mono(&input_samples, input_rate, SAMPLE_RATE);
    let delay_caps_ms = [60_u32, 80, 120, 180, 250];

    let mut rows = Vec::new();
    for profile in sweep_profiles() {
        for &max_delay_ms in &delay_caps_ms {
            let run =
                run_impairment_harness_with_delay_bounds(&input_48k, &profile, 20, max_delay_ms)?;
            rows.push(make_row(&profile, max_delay_ms, &run));
        }
    }

    if let Some(parent) = csv_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_csv(&csv_path, &rows)?;

    println!("input: {}", input_path.display());
    println!("csv: {}", csv_path.display());
    println!("input_rate_hz: {input_rate}");
    for row in &rows {
        println!(
            "{} max_delay={}ms target={}ms conceal={} expand_q14={} rms_ratio={:.3}",
            row.profile_name,
            row.max_delay_ms,
            row.target_delay_ms,
            row.concealed_samples,
            row.expand_rate_q14,
            row.rms_ratio
        );
    }

    Ok(())
}

fn make_row(profile: &ImpairmentProfile, max_delay_ms: u32, run: &HarnessOutput) -> SweepRow {
    let input_rms = rms(&run.input_samples);
    let output_rms = rms(&run.output_samples);
    SweepRow {
        profile_name: profile.name,
        max_delay_ms,
        input_rms,
        output_rms,
        rms_ratio: if input_rms > 0.0 {
            output_rms / input_rms
        } else {
            0.0
        },
        preferred_buffer_size_ms: run.stats.preferred_buffer_size_ms,
        target_delay_ms: run.stats.target_delay_ms,
        current_buffer_size_ms: run.stats.current_buffer_size_ms,
        expand_rate_q14: run.stats.expand_rate,
        accelerate_rate_q14: run.stats.accelerate_rate,
        concealed_samples: run.stats.concealed_samples,
        packets_awaiting_decode: run.stats.packets_awaiting_decode,
    }
}

fn write_csv(path: &PathBuf, rows: &[SweepRow]) -> Result<()> {
    let mut out = String::from(
        "profile,max_delay_ms,input_rms,output_rms,rms_ratio,preferred_buffer_size_ms,target_delay_ms,current_buffer_size_ms,expand_rate_q14,accelerate_rate_q14,concealed_samples,packets_awaiting_decode\n",
    );
    for row in rows {
        out.push_str(&format!(
            "{},{},{:.6},{:.6},{:.6},{},{},{},{},{},{},{}\n",
            row.profile_name,
            row.max_delay_ms,
            row.input_rms,
            row.output_rms,
            row.rms_ratio,
            row.preferred_buffer_size_ms,
            row.target_delay_ms,
            row.current_buffer_size_ms,
            row.expand_rate_q14,
            row.accelerate_rate_q14,
            row.concealed_samples,
            row.packets_awaiting_decode
        ));
    }
    fs::write(path, out).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn read_wav_mono_f32(path: &PathBuf) -> Result<(Vec<f32>, u32)> {
    let mut reader =
        WavReader::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let spec = reader.spec();
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
        _ => anyhow::bail!(
            "unsupported wav format: {:?} {} bits",
            spec.sample_format,
            spec.bits_per_sample
        ),
    };
    Ok((samples, spec.sample_rate))
}

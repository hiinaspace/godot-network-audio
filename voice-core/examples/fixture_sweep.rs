use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
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
    full_output_rms_ratio: f32,
    measured_output_rms: f32,
    measured_rms_ratio: f32,
    avg_preferred_buffer_size_ms: f32,
    avg_target_delay_ms: f32,
    avg_current_buffer_size_ms: f32,
    avg_expand_rate_q14: f32,
    avg_accelerate_rate_q14: f32,
    concealed_samples_delta: u64,
    final_packets_awaiting_decode: usize,
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
    let wav_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/fixture_sweep_wavs"));

    let (input_samples, input_rate) = read_wav_mono_f32(&input_path)?;
    let input_48k = linear_resample_mono(&input_samples, input_rate, SAMPLE_RATE);
    let delay_caps_ms = [60_u32, 80, 120, 180, 250];

    fs::create_dir_all(&wav_dir)
        .with_context(|| format!("failed to create {}", wav_dir.display()))?;

    let mut rows = Vec::new();
    for profile in sweep_profiles() {
        for &max_delay_ms in &delay_caps_ms {
            let run =
                run_impairment_harness_with_delay_bounds(&input_48k, &profile, 20, max_delay_ms)?;
            let wav_path = wav_dir.join(format!("{}_{}ms.wav", profile.name, max_delay_ms));
            write_wav_mono_f32(&wav_path, &run.output_samples, SAMPLE_RATE)?;
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
    println!("wav_dir: {}", wav_dir.display());
    println!("input_rate_hz: {input_rate}");
    for row in &rows {
        println!(
            "{} max_delay={}ms avg_pref_buf={:.1}ms conceal_delta={} avg_expand_q14={:.1} measured_rms_ratio={:.3}",
            row.profile_name,
            row.max_delay_ms,
            row.avg_preferred_buffer_size_ms,
            row.concealed_samples_delta,
            row.avg_expand_rate_q14,
            row.measured_rms_ratio
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
        full_output_rms_ratio: if input_rms > 0.0 {
            output_rms / input_rms
        } else {
            0.0
        },
        measured_output_rms: run.metrics.measured_output_rms,
        measured_rms_ratio: if input_rms > 0.0 {
            run.metrics.measured_output_rms / input_rms
        } else {
            0.0
        },
        avg_preferred_buffer_size_ms: run.metrics.avg_preferred_buffer_size_ms,
        avg_target_delay_ms: run.metrics.avg_target_delay_ms,
        avg_current_buffer_size_ms: run.metrics.avg_current_buffer_size_ms,
        avg_expand_rate_q14: run.metrics.avg_expand_rate_q14,
        avg_accelerate_rate_q14: run.metrics.avg_accelerate_rate_q14,
        concealed_samples_delta: run.metrics.concealed_samples_delta,
        final_packets_awaiting_decode: run.final_stats.packets_awaiting_decode,
    }
}

fn write_csv(path: &PathBuf, rows: &[SweepRow]) -> Result<()> {
    let mut out = String::from(
        "profile,max_delay_ms,input_rms,output_rms,full_output_rms_ratio,measured_output_rms,measured_rms_ratio,avg_preferred_buffer_size_ms,avg_target_delay_ms,avg_current_buffer_size_ms,avg_expand_rate_q14,avg_accelerate_rate_q14,concealed_samples_delta,final_packets_awaiting_decode\n",
    );
    for row in rows {
        out.push_str(&format!(
            "{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.3},{:.3},{:.3},{:.3},{:.3},{},{}\n",
            row.profile_name,
            row.max_delay_ms,
            row.input_rms,
            row.output_rms,
            row.full_output_rms_ratio,
            row.measured_output_rms,
            row.measured_rms_ratio,
            row.avg_preferred_buffer_size_ms,
            row.avg_target_delay_ms,
            row.avg_current_buffer_size_ms,
            row.avg_expand_rate_q14,
            row.avg_accelerate_rate_q14,
            row.concealed_samples_delta,
            row.final_packets_awaiting_decode
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

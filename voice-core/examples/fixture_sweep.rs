use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use hound::{SampleFormat, WavReader};
use voice_core::fixture_harness::{
    linear_resample_mono, rms, run_impairment_harness, sweep_profiles, HarnessOutput,
    ImpairmentProfile, SAMPLE_RATE,
};

#[derive(Debug)]
struct SweepRow {
    profile_name: &'static str,
    input_rms: f32,
    output_rms: f32,
    rms_ratio: f32,
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
    let svg_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/fixture_sweep.svg"));

    let (input_samples, input_rate) = read_wav_mono_f32(&input_path)?;
    let input_48k = linear_resample_mono(&input_samples, input_rate, SAMPLE_RATE);

    let mut rows = Vec::new();
    for profile in sweep_profiles() {
        let run = run_impairment_harness(&input_48k, &profile)?;
        rows.push(make_row(&profile, &run));
    }

    if let Some(parent) = csv_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = svg_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    write_csv(&csv_path, &rows)?;
    write_svg(&svg_path, &rows)?;

    println!("input: {}", input_path.display());
    println!("csv: {}", csv_path.display());
    println!("svg: {}", svg_path.display());
    println!("input_rate_hz: {input_rate}");
    for row in &rows {
        println!(
            "{} delay={}ms conceal={} expand_q14={} rms_ratio={:.3}",
            row.profile_name,
            row.target_delay_ms,
            row.concealed_samples,
            row.expand_rate_q14,
            row.rms_ratio
        );
    }

    Ok(())
}

fn make_row(profile: &ImpairmentProfile, run: &HarnessOutput) -> SweepRow {
    let input_rms = rms(&run.input_samples);
    let output_rms = rms(&run.output_samples);
    SweepRow {
        profile_name: profile.name,
        input_rms,
        output_rms,
        rms_ratio: if input_rms > 0.0 {
            output_rms / input_rms
        } else {
            0.0
        },
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
        "profile,input_rms,output_rms,rms_ratio,target_delay_ms,current_buffer_size_ms,expand_rate_q14,accelerate_rate_q14,concealed_samples,packets_awaiting_decode\n",
    );
    for row in rows {
        out.push_str(&format!(
            "{},{:.6},{:.6},{:.6},{},{},{},{},{},{}\n",
            row.profile_name,
            row.input_rms,
            row.output_rms,
            row.rms_ratio,
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

fn write_svg(path: &PathBuf, rows: &[SweepRow]) -> Result<()> {
    let width = 820.0_f32;
    let height = 420.0_f32;
    let margin_left = 70.0_f32;
    let margin_right = 30.0_f32;
    let margin_top = 30.0_f32;
    let margin_bottom = 60.0_f32;
    let plot_width = width - margin_left - margin_right;
    let plot_height = height - margin_top - margin_bottom;
    let max_delay = rows
        .iter()
        .map(|row| row.target_delay_ms)
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let max_concealed = rows
        .iter()
        .map(|row| row.concealed_samples)
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let slot_w = plot_width / rows.len().max(1) as f32;
    let bar_w = slot_w * 0.28;

    let mut svg = String::new();
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<style>
text {{ font-family: monospace; fill: #1a1a1a; font-size: 12px; }}
.title {{ font-size: 16px; font-weight: bold; }}
.axis {{ stroke: #444; stroke-width: 1; }}
.grid {{ stroke: #ddd; stroke-width: 1; }}
</style>
<rect width="100%" height="100%" fill="#faf8f2"/>
<text class="title" x="{margin_left}" y="20">NetEq impairment sweep</text>
"##
    ));

    for i in 0..=4 {
        let y = margin_top + plot_height * (i as f32 / 4.0);
        svg.push_str(&format!(
            r#"<line class="grid" x1="{margin_left}" y1="{y}" x2="{}" y2="{y}"/>"#,
            width - margin_right
        ));
    }
    svg.push_str(&format!(
        r#"<line class="axis" x1="{margin_left}" y1="{margin_top}" x2="{margin_left}" y2="{}"/>
<line class="axis" x1="{margin_left}" y1="{}" x2="{}" y2="{}"/>
"#,
        height - margin_bottom,
        height - margin_bottom,
        width - margin_right,
        height - margin_bottom
    ));

    for (i, row) in rows.iter().enumerate() {
        let x_center = margin_left + slot_w * (i as f32 + 0.5);
        let delay_h = if max_delay > 0.0 {
            (row.target_delay_ms as f32 / max_delay) * plot_height
        } else {
            0.0
        };
        let conceal_h = if max_concealed > 0.0 {
            (row.concealed_samples as f32 / max_concealed) * plot_height
        } else {
            0.0
        };
        let y_base = height - margin_bottom;
        let x1 = x_center - bar_w - 4.0;
        let x2 = x_center + 4.0;
        let y1 = y_base - delay_h;
        let y2 = y_base - conceal_h;

        svg.push_str(&format!(
            r##"<rect x="{x1}" y="{y1}" width="{bar_w}" height="{delay_h}" fill="#2f6fed"/>
<rect x="{x2}" y="{y2}" width="{bar_w}" height="{conceal_h}" fill="#e86a33"/>
<text transform="translate({x_center}, {}) rotate(-30)" text-anchor="end">{}</text>
<text x="{x1}" y="{}" text-anchor="middle">{}</text>
"##,
            height - margin_bottom + 22.0,
            row.profile_name,
            y1 - 6.0,
            row.target_delay_ms
        ));
    }

    svg.push_str(&format!(
        r##"<rect x="{}" y="{}" width="12" height="12" fill="#2f6fed"/><text x="{}" y="{}">target delay ms</text>
<rect x="{}" y="{}" width="12" height="12" fill="#e86a33"/><text x="{}" y="{}">concealed samples</text>
</svg>
"##,
        width - 220.0,
        18.0,
        width - 202.0,
        28.0,
        width - 220.0,
        38.0,
        width - 202.0,
        48.0
    ));

    fs::write(path, svg).with_context(|| format!("failed to write {}", path.display()))?;
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

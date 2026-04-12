#!/usr/bin/env -S uv run
# /// script
# dependencies = [
#   "matplotlib>=3.8",
#   "numpy>=2.0",
#   "scipy>=1.13",
# ]
# ///

from __future__ import annotations

import sys
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np
from scipy.io import wavfile


def main() -> int:
    if len(sys.argv) not in (3, 4):
        print(
            "usage: plot_demo_io_spectrograms.py INPUT_WAV OUTPUT_WAV [OUTPUT_PNG]",
            file=sys.stderr,
        )
        return 2

    input_wav = Path(sys.argv[1])
    output_wav = Path(sys.argv[2])
    output_png = (
        Path(sys.argv[3])
        if len(sys.argv) == 4
        else output_wav.parent / "demo_io_spectrograms.png"
    )

    input_rate, input_samples = load_mono(input_wav)
    output_rate, output_samples = load_mono(output_wav)

    fig, axes = plt.subplots(
        2,
        1,
        figsize=(12, 7),
        sharex=False,
        constrained_layout=True,
    )

    render_specgram(axes[0], input_samples, input_rate, f"Input Virtual Mic: {input_wav.name}")
    render_specgram(axes[1], output_samples, output_rate, f"Godot Output Capture: {output_wav.name}")

    fig.suptitle("Godot Demo IO Spectrograms", fontsize=14)
    output_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_png, dpi=160)
    print(output_png)
    return 0


def render_specgram(ax: plt.Axes, samples: np.ndarray, sample_rate: int, title: str) -> None:
    ax.specgram(
        samples,
        Fs=sample_rate,
        NFFT=1024,
        noverlap=768,
        cmap="magma",
        scale="dB",
    )
    ax.set_ylim(0, 8000)
    ax.set_ylabel("Hz")
    ax.set_xlabel("Seconds")
    ax.set_title(title)


def load_mono(path: Path) -> tuple[int, np.ndarray]:
    sample_rate, samples = wavfile.read(path)
    if samples.ndim > 1:
        samples = samples.mean(axis=1)
    samples = samples.astype(np.float32, copy=False)
    peak = np.max(np.abs(samples)) if samples.size else 0.0
    if peak > 1.5:
        samples /= peak
    return sample_rate, samples


if __name__ == "__main__":
    raise SystemExit(main())

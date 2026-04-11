#!/usr/bin/env -S uv run
# /// script
# dependencies = [
#   "matplotlib>=3.8",
#   "numpy>=2.0",
#   "scipy>=1.13",
# ]
# ///

from __future__ import annotations

import math
import re
import sys
from dataclasses import dataclass
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np
from scipy.io import wavfile


NAME_RE = re.compile(r"^(?P<profile>.+)_(?P<delay>\d+)ms\.wav$")
PROFILE_ORDER = ["clean", "good_network", "normal_wan", "bad_wan", "stress"]


@dataclass(frozen=True)
class WavEntry:
    path: Path
    profile: str
    delay_ms: int


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print(
            "usage: plot_fixture_spectrograms.py WAV_DIR [OUTPUT_PNG]",
            file=sys.stderr,
        )
        return 2

    wav_dir = Path(sys.argv[1])
    output_png = (
        Path(sys.argv[2])
        if len(sys.argv) == 3
        else wav_dir.parent / "fixture_spectrograms.png"
    )

    entries = collect_entries(wav_dir)
    if not entries:
        print(f"no matching wavs in {wav_dir}", file=sys.stderr)
        return 1

    profiles = sorted(
        {entry.profile for entry in entries},
        key=lambda name: (
            PROFILE_ORDER.index(name) if name in PROFILE_ORDER else len(PROFILE_ORDER),
            name,
        ),
    )
    delays = sorted({entry.delay_ms for entry in entries})
    entry_map = {(entry.profile, entry.delay_ms): entry for entry in entries}

    fig, axes = plt.subplots(
        len(profiles),
        len(delays),
        figsize=(3.1 * len(delays), 2.2 * len(profiles)),
        squeeze=False,
        constrained_layout=True,
    )

    for row, profile in enumerate(profiles):
        for col, delay_ms in enumerate(delays):
            ax = axes[row][col]
            entry = entry_map.get((profile, delay_ms))
            if entry is None:
                ax.axis("off")
                continue

            sample_rate, samples = load_mono(entry.path)
            ax.specgram(
                samples,
                Fs=sample_rate,
                NFFT=1024,
                noverlap=768,
                cmap="magma",
                scale="dB",
            )
            ax.set_ylim(0, 8000)
            ax.set_xticks([])
            ax.set_yticks([])
            if row == 0:
                ax.set_title(f"{delay_ms} ms")
            if col == 0:
                ax.set_ylabel(profile)
            ax.text(
                0.98,
                0.04,
                f"{samples.shape[0] / sample_rate:.1f}s",
                transform=ax.transAxes,
                ha="right",
                va="bottom",
                color="white",
                fontsize=8,
                bbox={"facecolor": "black", "alpha": 0.35, "pad": 2, "edgecolor": "none"},
            )

    fig.suptitle("Fixture Sweep Spectrograms", fontsize=14)
    output_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_png, dpi=160)
    print(output_png)
    return 0


def collect_entries(wav_dir: Path) -> list[WavEntry]:
    entries: list[WavEntry] = []
    for path in sorted(wav_dir.glob("*.wav")):
        match = NAME_RE.match(path.name)
        if not match:
            continue
        entries.append(
            WavEntry(
                path=path,
                profile=match.group("profile"),
                delay_ms=int(match.group("delay")),
            )
        )
    return entries


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

#!/usr/bin/env python3
from __future__ import annotations

import csv
import sys
from collections import defaultdict
from pathlib import Path

import matplotlib.pyplot as plt


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print("usage: plot_fixture_sweep.py INPUT_CSV [OUTPUT_PNG]", file=sys.stderr)
        return 2

    input_csv = Path(sys.argv[1])
    output_png = (
        Path(sys.argv[2]) if len(sys.argv) == 3 else input_csv.with_suffix(".png")
    )

    rows = list(csv.DictReader(input_csv.open()))
    if not rows:
        print(f"no rows in {input_csv}", file=sys.stderr)
        return 1

    grouped: dict[str, list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        grouped[row["profile"]].append(row)

    fig, axes = plt.subplots(3, 1, figsize=(10, 10), sharex=True, constrained_layout=True)
    ax_buffer, ax_conceal, ax_expand = axes

    for profile, entries in grouped.items():
        entries.sort(key=lambda row: int(row["max_delay_ms"]))
        x = [int(row["max_delay_ms"]) for row in entries]
        y_buffer = [float(row["avg_preferred_buffer_size_ms"]) for row in entries]
        y_conceal = [int(row["concealed_samples_delta"]) for row in entries]
        y_expand = [float(row["avg_expand_rate_q14"]) / 16384.0 for row in entries]
        ax_buffer.plot(x, y_buffer, marker="o", label=profile)
        ax_conceal.plot(x, y_conceal, marker="o", label=profile)
        ax_expand.plot(x, y_expand, marker="o", label=profile)

    ax_buffer.set_title("NetEq preferred buffer size vs configured max delay")
    ax_buffer.set_ylabel("preferred buffer (ms)")
    ax_buffer.grid(True, alpha=0.3)
    ax_buffer.legend()

    ax_conceal.set_title("Concealed samples vs configured max delay")
    ax_conceal.set_ylabel("concealed samples")
    ax_conceal.grid(True, alpha=0.3)

    ax_expand.set_title("Expand rate vs configured max delay")
    ax_expand.set_xlabel("configured max delay (ms)")
    ax_expand.set_ylabel("expand rate")
    ax_expand.grid(True, alpha=0.3)

    output_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_png, dpi=160)
    print(output_png)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

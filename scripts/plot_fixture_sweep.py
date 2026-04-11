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

    fig, axes = plt.subplots(2, 1, figsize=(10, 8), sharex=True, constrained_layout=True)
    ax_delay, ax_conceal = axes

    for profile, entries in grouped.items():
        entries.sort(key=lambda row: int(row["max_delay_ms"]))
        x = [int(row["max_delay_ms"]) for row in entries]
        y_delay = [int(row["target_delay_ms"]) for row in entries]
        y_conceal = [int(row["concealed_samples"]) for row in entries]
        ax_delay.plot(x, y_delay, marker="o", label=profile)
        ax_conceal.plot(x, y_conceal, marker="o", label=profile)

    ax_delay.set_title("NetEq target delay vs configured max delay")
    ax_delay.set_ylabel("target delay (ms)")
    ax_delay.grid(True, alpha=0.3)
    ax_delay.legend()

    ax_conceal.set_title("Concealed samples vs configured max delay")
    ax_conceal.set_xlabel("configured max delay (ms)")
    ax_conceal.set_ylabel("concealed samples")
    ax_conceal.grid(True, alpha=0.3)

    output_png.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_png, dpi=160)
    print(output_png)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

# Opus 1.6.1 and DRED experiment

Date: 2026-09-04

The current Linux path dynamically links Debian libopus 1.5.2. This experiment
compared it with official libopus 1.6.1 without changing the production
`audiopus` API, then exercised DRED through an isolated maintained binding.
Raw JSONL and whole-party JSON results are retained at
`gna-sim:/work/projects/godot-network-audio/target/opus-results/`.

## Conclusions

- Do not blindly replace the deployed 1.5.2 build yet. Libopus 1.6.1 encoded
  voiced frames about 6% more slowly, the 3-second-voice/1-second-silence
  workload about 17% more slowly, and continuous silence about 73% more slowly.
  Packet counts and payload bytes were effectively unchanged. This reproduces
  the shape of the upstream 1.6 silence-path performance report.
- A DRED-capable 1.6.1 build with DRED disabled passed the representative
  32-participant game-interest smoke without transport or NetEq errors. Its
  end-to-end CPU and latency ranges overlapped 1.5.2 and are too noisy to rank.
- DRED did not fit at the project's current 16 kb/s target. At 24 kb/s and
  above, it recovered a scripted 100 ms loss burst in the functional smoke,
  with substantial encoder CPU cost and bitrate-dependent wire cost.
- DRED should be treated as an adaptive policy experiment, not as a free codec
  upgrade. It requires a nonzero expected-loss setting and dedicated receive
  parsing/decoding. The current `audiopus` wrapper does not expose that receive
  API.

## Focused 1.5.2 versus 1.6.1 encode benchmark

Each cell is the median of five interleaved 10,000-frame runs on `gna-sim`.
The production `VoiceEncoder` used 48 kHz mono, 20 ms frames, VBR, and 16 kb/s.

| Workload | DTX | 1.5.2 ns/frame | 1.6.1 ns/frame | 1.6.1 / 1.5.2 |
|---|---:|---:|---:|---:|
| voiced | off | 166,382 | 176,372 | 1.060 |
| voiced | on | 166,897 | 176,891 | 1.060 |
| silence | off | 146,375 | 252,733 | 1.727 |
| silence | on | 145,536 | 253,121 | 1.739 |
| 3 s voiced / 1 s silence | off | 179,755 | 209,389 | 1.165 |
| 3 s voiced / 1 s silence | on | 179,126 | 209,031 | 1.167 |

The compared 1.6.1 library was compiled with DRED support but DRED was disabled
at runtime. A second 1.6.1 build without DRED produced almost identical encoder
ratios, but it is not a fair receiver-memory comparison with Debian's build.

## DRED smoke

Each cell is the median of five 10,000-frame runs using libopus 1.6.1, 10%
expected packet loss, VBR, and a one-second maximum DRED duration. “Recovered”
counts generated frames for a five-packet/100 ms scripted gap; it is not a
quality score.

| Target | DRED off wire | DRED on wire | Wire ratio | CPU ratio | Recovered |
|---:|---:|---:|---:|---:|---:|
| 16 kb/s | 16.46 kb/s | 16.46 kb/s | 1.000 | 1.252 | 0 / 5 |
| 24 kb/s | 24.42 kb/s | 28.00 kb/s | 1.147 | 1.215 | 5 / 5 |
| 32 kb/s | 32.40 kb/s | 33.95 kb/s | 1.048 | 2.804 | 5 / 5 |
| 48 kb/s | 48.40 kb/s | 50.25 kb/s | 1.038 | 1.480 | 5 / 5 |
| 64 kb/s | 64.40 kb/s | 65.21 kb/s | 1.012 | 1.454 | 5 / 5 |

The 32 kb/s CPU discontinuity is plausible codec-mode switching and should be
retested with recorded speech before being generalized. At 16 kb/s, libopus
performed DRED analysis but emitted no redundancy, so the 25% CPU increase
bought no recovery in this signal.

## Representative 32-participant control

Three seeds per version used direct sender-filtered delivery, four rotating
talkers, seven listeners per talker, pooled receivers, DTX, eight runtime
workers, and 12 seconds of media. Every run had zero send errors, missing
datagrams, and NetEq receiver errors. The median process CPU was 33.0 seconds
for 1.5.2 and 30.0 seconds for 1.6.1, but their ranges were 29.1-35.7 and
29.0-37.1 seconds; this does not support ranking them.

Median maximum RSS was 189.8 MiB for Debian 1.5.2 and 175.2 MiB for the
DRED-capable 1.6.1 build. This is explained by compile-time state size rather
than DRED behavior: mono decoder state was 141,108 bytes versus 87,748 bytes.
The first 1.6.1 build omitted neural/deep PLC and used only 18,468-byte decoder
state, creating a misleading ~38 MiB apparent win. Those initial whole-party
numbers must not be used as a version comparison.

## Limits and follow-up

- The deterministic signal is only a stable CPU/packet stimulus. It does not
  establish intelligibility or perceptual improvement.
- VBR output can exceed the nominal target; DRED overhead is not constant.
- Compile options materially affect Opus state size. Record library version,
  feature flags, and encoder controls for future comparisons.
- A real DRED evaluation needs several recorded voices, burst lengths and loss
  rates, aligned decoded WAVs, and listening or a full-reference metric. That
  work is only warranted if DRED remains attractive after the broader harness
  review.

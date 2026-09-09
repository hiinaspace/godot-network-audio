# Isolated mixer wait diagnostic

Date: 2026-09-09. Base source `98217ae`, unchanged Godot 4.7.2 and extension
binaries, PulseAudio 17.0. Receiver gna-sim; fleet gna-loadgen; cc-0 only
orchestrated. Harness changes are in the working tree.

## Finding

The reproducible second-long startup callback gap is a PulseAudio null-sink
latency artifact. An untraced control reproduced 1,247 ms between callback exit
and next entry; tracing reproduced 1,245 ms. During the gap, the apparent audio
driver thread continuously performs nonblocking PulseAudio polls and ~1 ms
sleeps. There is no matching second-long futex wait or scheduling absence.

A separate untraced reproduction with `pactl` observations reports actual sink
latency initially at 1,868 ms despite configured latency of 10.667 ms. Actual
latency counts down through 1,523 / 1,198 / 873 / 548 / 223 ms before settling
below 11 ms. The Godot stream is uncorked with about 32 ms buffered during this
interval. Callback progress resumes as the sink backlog drains.

This matches the relevant source behavior:

- [Godot 4.7.2 PulseAudio driver](https://github.com/godotengine/godot/blob/4.7.2-stable/drivers/pulseaudio/audio_driver_pulseaudio.cpp#L373):
  another mix occurs after pending output has been written; when neither reads
  nor writes progress, the driver sleeps 1 ms and polls again.
- [PulseAudio 17 null sink](https://github.com/pulseaudio/pulseaudio/blob/v17.0/src/modules/module-null-sink.c):
  normal maximum latency is two seconds. `norewinds=1` disables rewinds and uses
  a 50 ms maximum instead. Requested latency updates the block size; the render
  loop tracks an already-advanced sink timestamp.

The source comparison supports the backlog explanation; syscall tracing alone
cannot identify the exact userspace branch. The controlled sink-setting change
below supplies the stronger causal evidence. This does not establish a Godot
engine bug or imply production audio devices need this setting.

## Controlled change

The isolated harness now accepts `--norewinds`, which changes only the temporary
null sink's module argument. `--trace` records receiver syscalls by thread in
`godot_waits.*`; the manifest records both settings. The legacy sink remains
available by omitting `--norewinds` so the original failure stays reproducible.

| Control | Callback gap max | Callback execution max | First output max, receiver-local | Concealed samples |
|---|---:|---:|---:|---:|
| Default sink, fixed 3D | 1,247 ms | 5.2 ms | 1,321 ms | 0 |
| Default sink, fixed 3D, strace | 1,245 ms | 5.3 ms | 1,326 ms | 0 |
| Default sink, fixed 3D, pactl observer | 1,260 ms | 18.3 ms | 1,352 ms | 0 |
| No rewinds, fixed 3D | 21.2 ms | 5.5 ms | 111 ms | 0 |
| No rewinds, fixed 2D | 22.1 ms | 10.4 ms | 98.6 ms | 1,440 |
| No rewinds, fixed 3D, strace | 17.3 ms | 5.6 ms | 117 ms | 480 |
| No rewinds, 31-peer/7-active 3D churn | 28.1 ms | 12.5 ms | 132 ms | 960 |

All listed runs delivered every expected packet, with no queue drops, receiver
errors, or sender deadlines over 20 ms. Churn delivered 5,481 packets, all 38
connect/disconnect events and 28 talker activations; collateral concealment was
zero. The small concealment counts in three controls remain real diagnostics;
this is not a claim of perfect playback or a new capacity benchmark.

The separate no-rewinds `pactl` observation sampled actual sink latency at most
10.583 ms (it was started after run launch, not an exhaustive startup maximum).

## Measurement cautions

Use `callback_gap_us_max`, the direct callback entry/exit instrumentation.
The older `audio_callback_gap_ms_max` main-loop heuristic still reports around
190–202 ms in corrected runs: it counts periods across lifecycle/startup states
and does not reset when nothing is playing. It is not proof of a stalled audio
thread; future summaries should make that distinction explicit.

A 10 ms / -45 dBFS RMS scan of the fixed controls' captured WAVs found no silent
windows between first and last active windows. However, default-sink captures
contain only about 14 seconds of active audio versus about 15 seconds with no
rewinds, and begin almost immediately with tone. Capture sample zero is not
Godot launch time. Neither missing leading silence nor zero concealment rules
out startup delay; do not use these WAV offsets as end-to-end latency.

`strace` adds overhead; traced CPU/resource figures must not be used for scale
comparisons. The final harness verifies the sampled PID is the actual engine,
not strace's transient capability probe, and saves its worker source/hash.

## Reproduce and next work

```sh
# Run on cc-0, workload on the two workers only.
python3 scripts/run_godot_isolated.py --fixed --spatial 3 --trace
python3 scripts/run_godot_isolated.py --fixed --spatial 3 --norewinds
python3 scripts/run_godot_isolated.py --spatial 3 --norewinds
```

The bounded startup diagnostic is complete, and the small churn control no
longer has the second-long mixer pause. Use `--norewinds` for subsequent pod
controls and preserve the default-sink case as a diagnostic reproduction.
Do not extrapolate these short runs to long sessions, real hardware, or VR.
Next implementation work: bound retired receive-player lifetime and measure
safe reclamation with this corrected harness, then real desktop 2–3-person
integration. Target ~16 participants when investigating remaining scale
constraints; 32 remains a characterization point.

## Artifacts

All directories below are on gna-sim under
`/work/projects/godot-network-audio/target/godot-gate/isolated/`:

- `fixed-3d-1788985746648270774-0`: fresh default control.
- `fixed-3d-1788985776224875869-0`: default sink with syscall traces;
  apparent audio-driver TID 14589, main PID 14568.
- `fixed-3d-1788985867241695829-0`: default sink plus
  `pulse_observations.jsonl` latency samples.
- `fixed-3d-1788985908567465569-0`: no-rewinds 3D, latency observations.
- `fixed-2d-1788985943023307967-0`: no-rewinds 2D.
- `churn-3d-1788985985972789736-0`: no-rewinds churn.
- `fixed-3d-1788986015386520376-0`: final no-rewinds traced control and
  verification of saved worker source/hash and actual-engine PID sampling.

Each run retains summary, manifests, receiver and sender telemetry, and WAV.
Selected fixed controls also retain `wav_continuity.json`.

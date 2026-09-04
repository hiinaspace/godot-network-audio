# Opus 1.6 DRED smoke benchmark

This crate isolates the Opus 1.6 DRED API from `voice-core`'s current
`audiopus` binding. It builds libopus 1.6.1 from source with DRED enabled and
prints one JSON object per tested bitrate.

```sh
cargo run --release -p codec-bench -- 10000
```

The optional argument is the number of 20 ms frames per encode measurement.
Each bitrate is measured with DRED disabled and enabled. Both cases use 10%
expected packet loss; the enabled case retains up to one second of DRED. The
recovery smoke drops five consecutive packets (100 ms), parses redundancy from
the next packet, and reports the count and RMS of generated recovery frames.

This is a functional, CPU, and wire-size benchmark. Its deterministic
speech-like synthetic signal is not suitable for perceptual quality claims.
Use recorded speech and aligned output scoring before making a DRED quality or
bitrate-policy decision.

The source build requires Rust's `llvm-tools` component and `libclang-dev`.
`shiguredo_opus` and the system-backed `audiopus` crate must not be linked into
the same executable: their native library search/link requirements conflict.
The production-version comparison therefore lives in
`voice-core/examples/opus_encode_bench.rs`.

# Agent Notes

- After Rust code changes, run:
  - `source ~/.cargo/env && cargo fmt --all`
  - `source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings`
  - `source ~/.cargo/env && cargo check --workspace`
- Keep `voice-core` sans-IO. Godot-specific code belongs in `gdext/`.
- For receive audio, keep NetEq ownership on playback/audio-thread side. Main/network-thread code should only enqueue packets plus arrival metadata.
- Prefer monotonic arrival timestamps. If transport code already has a monotonic receive time, pass it through instead of restamping in Godot.

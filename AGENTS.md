# Agent Notes

- After Rust code changes, run:
  - `source ~/.cargo/env && cargo fmt --all`
  - `source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings`
  - `source ~/.cargo/env && cargo check --workspace`
- For long-running commands from Codex (`claude -p`, Godot harness runs, etc.), set `yield_time_ms` to roughly the expected wall time so the exec session stays foregrounded instead of yielding immediately.
- Keep `voice-core` sans-IO. Godot-specific code belongs in `gdext/`.
- For receive audio, keep NetEq ownership on playback/audio-thread side. Main/network-thread code should only enqueue packets plus arrival metadata.
- Prefer monotonic arrival timestamps. If transport code already has a monotonic receive time, pass it through instead of restamping in Godot.

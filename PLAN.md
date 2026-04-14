# godot-network-audio — tactical plan

This file tracks current status and the next implementation milestones.
Durable architecture notes live in `DESIGN.md`.

## Current status

Implemented and working at a prototype level:
- `voice-core` crate with Opus encode/decode, DTX, packet format, NetEq-backed receive path, and basic resampling helpers.
- `gdext` crate with:
  - `NetworkAudioSender` — encode thread calls send handler directly, no `_process()` dependence
  - `AudioStreamNetwork` — receive handler on iroh tokio thread, no `_process()` dependence
  - `IrohVoiceTransport` — iroh 0.97.0 QUIC datagrams, merged into the main extension under `iroh-transport` feature flag
- Two-process iroh demo under `example_iroh/`, with PipeWire harness (`run_iroh_demo_pipewire_harness.sh`)
- Harness tools:
  - virtual mic + output capture via PulseAudio null sinks
  - JSONL traces from both processes
  - `plot_demo_stats.py` with `--sender`/`--receiver` mode for separate traces, full 5-panel NetEq detail
  - `plot_demo_io_spectrograms.py` for input/output comparison
- local `neteq` override for more trustworthy receiver stats while we validate behavior

Known-good behavior (loopback, no impairment):
- both send and receive paths fully decoupled from `_process()` cadence (verified at 1 fps receiver)
- `avg_packet_interval` ≈ 20 ms, `max_enqueue_interval` ≈ 40 ms on clean loopback
- zero concealment at `min_delay_ms=80`; brief OS-jitter dropouts at `min_delay_ms=20`

Open gaps:
- all testing so far uses direct loopback — no simulated network impairment yet
- iroh currently uses the N0 relay preset; for netem tests we need relay-free direct-UDP-only mode with explicit bind address, otherwise netem won't shape all traffic
- the sender egress API (`packet_ready` signal / `attach_sender` direct handler) works but is still somewhat demo-oriented
- docs partially out of date with the merged-extension architecture

## Next milestone

### M2: netem network impairment sweep

Goal: characterise voice quality across a range of simulated network conditions using
`tc netem` on a local veth pair.  Move from "inspect individual traces" to "sweep
profiles and plot aggregates".

#### Overview

We create a veth pair (`veth-gna-tx`/`veth-gna-rx`) and bind each Godot process to
one end.  `tc netem` is applied to each half of the pair so all iroh UDP traffic is
shaped.  We disable iroh's relay and DNS-pkarr machinery (which would route some
packets off-machine) so the veth is the only path.  We run a batch of profiles and
collect a summary CSV plus comparison plots.

#### Sub-task 1: tune defaults

- Change `AudioStreamNetwork.min_delay_ms` default from 20 → 80 (WebRTC's `K_START_DELAY_MS`; confirmed zero-concealment on clean loopback).
- Possibly raise `max_delay_ms` default from 120 → 200 to give NetEq more headroom under high-delay profiles.
- Document why in the code.

#### Sub-task 2: iroh local-only mode

Add to `VoiceIrohConfig`:
```rust
pub bind_addr: Option<SocketAddr>,   // None = bind all interfaces (current behaviour)
pub relay: bool,                     // false = Builder::empty(), no relay, no pkarr/DNS
```

In `VoiceIrohService::bind()`:
- If `relay=false`: use `Builder::empty()` (no relay, no external address lookup). Then:
  - call `.clear_ip_transports()` to drop the default `0.0.0.0` binding
  - call `.bind_addr(addr)?` to bind to the specific veth IP
  - `endpoint.addr().ip_addrs` will contain only the veth IP; the receiver JSON file is
    still used to exchange addresses, so no DNS/pkarr is needed
- If `relay=true` (default): keep `presets::N0` exactly as today

Add to `IrohVoiceTransport`:
- `#[func] start_endpoint_direct(bind_addr: GString) -> bool` — starts with relay=false,
  specific bind addr
- Or: read `GNA_IROH_BIND_ADDR` env var in the existing `start_endpoint()` and branch
  automatically (simpler for the harness)

`main.gd` picks up `GNA_IROH_BIND_ADDR`; if set, calls the direct-bind path.

Iroh 0.97.0 API notes:
- `Builder::empty()` — no relay, IPv4+IPv6 transports, no DNS
- `.clear_ip_transports()` — drops default 0.0.0.0/:: sockets
- `.bind_addr(addr: impl ToSocketAddr) -> Result<Builder, InvalidSocketAddr>` — fallible, sync
- `.relay_mode(RelayMode::Disabled)` — already implied by `Builder::empty()` but explicit is fine
- `.alpns(vec![...])` and `.bind().await` — unchanged

#### Sub-task 3: veth setup/teardown helper

`scripts/setup_gna_veth.sh [up|down]`:
- `up`: creates `veth-gna-tx` (10.99.0.1/24) + `veth-gna-rx` (10.99.0.2/24), brings both up.
  Idempotent: no-ops if already exists.
- `down`: deletes the pair (deleting one side removes both).
- Requires no root if `sudo` is available; the sweep script calls this automatically.

netem is applied and removed per-profile by the sweep script, not by this helper.

#### Sub-task 4: sweep harness

`scripts/run_iroh_netem_sweep.sh INPUT_WAV OUTPUT_DIR [RUN_SECONDS]`:

1. Call `setup_gna_veth.sh up` (idempotent).
2. For each profile in a table (see below), in sequence:
   a. Apply netem to both veth ends:
      ```bash
      sudo tc qdisc replace dev veth-gna-tx root netem delay ${D}ms ${J}ms \
           loss ${L}% duplicate ${DUP}% distribution normal
      sudo tc qdisc replace dev veth-gna-rx root netem delay ${D}ms ${J}ms \
           loss ${L}% duplicate ${DUP}% distribution normal
      ```
      (Apply to both sides for symmetric RTT shaping.)
   b. Run `run_iroh_demo_pipewire_harness.sh` with:
      ```bash
      GNA_IROH_BIND_ADDR_SENDER=10.99.0.1:0
      GNA_IROH_BIND_ADDR_RECEIVER=10.99.0.2:0
      GNA_DEMO_MIN_DELAY_MS=80
      ```
      Output goes into `OUTPUT_DIR/profile_NAME/`.
   c. Extract summary row from receiver trace (concealed_samples, expand_rate, etc.)
      into `OUTPUT_DIR/sweep_results.csv`.
   d. Remove netem: `sudo tc qdisc del dev veth-gna-{tx,rx} root`.
3. Call `plot_netem_sweep.py OUTPUT_DIR/sweep_results.csv OUTPUT_DIR/sweep.png`.
4. Print all output paths.

Proposed profiles:

| name          | delay | jitter | loss  | dup  | notes                        |
|---------------|-------|--------|-------|------|------------------------------|
| baseline      | 0ms   | 0ms    | 0%    | 0%   | clean veth                   |
| lan           | 2ms   | 0.5ms  | 0.05% | 0%   | good LAN                     |
| wan_good      | 30ms  | 5ms    | 0.1%  | 0%   | fast WAN                     |
| wan_mid       | 60ms  | 10ms   | 0.5%  | 0%   | typical WAN                  |
| wan_poor      | 100ms | 20ms   | 1%    | 0%   | mediocre WAN                 |
| mobile        | 80ms  | 30ms   | 2%    | 0.1% | mobile / congested           |
| bad           | 150ms | 40ms   | 4%    | 0%   | edge of usability            |

Run each for 15 s by default (enough for NetEq to adapt and for stats to stabilise).

#### Sub-task 5: sweep plot

`scripts/plot_netem_sweep.py SWEEP_CSV OUTPUT_PNG`:
- Reads the CSV (one row per profile).
- 4-panel bar chart:
  - Panel 1: concealment % of run duration, by profile
  - Panel 2: final expand_rate % (NetEq lifetime fraction spent in PLC), by profile
  - Panel 3: max_enqueue_interval_ms (worst-case gap seen), by profile
  - Panel 4: dropped_packets count, by profile
- Overlay the `min_delay_ms` floor as a reference line on the enqueue-gap panel.
- Write PNG, print path.

#### Acceptance

- Baseline and LAN profiles: zero concealment, expand_rate < 0.1%.
- WAN-good to WAN-mid: audible quality remains good, concealment < 1% of run.
- WAN-poor and mobile: audible with some degradation, no pathological buffer loops.
- Bad: measurable degradation captured in plots; provides a floor for what we call "acceptable loss".
- Sweep runs end-to-end in under 10 minutes on a developer machine.

### M3: sender egress API cleanup

Goal:
- make the transport handoff cleaner than the current demo-oriented loopback/signal split

Deliverables:
- a clear packet-source API from Rust suitable for non-main-thread transport consumers
- separation between:
  - local loopback/testing hooks
  - generic outbound packet stream
  - optional transport integrations like iroh

Acceptance:
- a game or sidecar transport does not need to depend on `process()` just to move packets out of the sender

### M4: doc and packaging cleanup

Goal:
- make the repo understandable to future maintainers and users

Deliverables:
- `DESIGN.md` stays architectural
- `PLAN.md` stays tactical
- README explains:
  - core transport-agnostic API
  - optional transport integrations
  - current caveats
- decide whether loopback/testing helpers stay in the main addon or move behind features/examples

## Parking lot

Useful but not next:
- HLMP integration example, with explicit caveats about polling/threading
- Steam Audio integration/fork pass
- output-side/runtime stats polish
- optional per-peer sender adaptation lanes for bitrate/FEC
- better harness lifecycle management for long-running multi-process runs

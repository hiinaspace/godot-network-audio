use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use godot_network_audio_iroh::{RemotePeer, VoiceIrohConfig, VoiceIrohService};
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use iroh_base::TransportAddr;
use serde::{Deserialize, Serialize};
use voice_core::{PacketFlags, VoiceEncoder, VoiceEncoderConfig};

const FRAME_SAMPLES: usize = 960;
const SAMPLE_RATE: f32 = 48_000.0;
const FRAME_DURATION: Duration = Duration::from_millis(20);
const DRAIN_FRAMES: u64 = 50;

#[derive(Deserialize)]
struct EndpointInfo {
    endpoint_id: String,
    ip_addrs: Vec<String>,
    relay_urls: Vec<String>,
}

struct Peer {
    service: VoiceIrohService,
    remote: RemotePeer,
    encoder: VoiceEncoder,
    phase_samples: u64,
    frequency_hz: f32,
    slot: usize,
    generation: u32,
    active: bool,
    closing_talkspurt: bool,
}

#[derive(Serialize)]
struct EventRecord<'a> {
    mono_usec: u64,
    unix_usec: u64,
    event: &'a str,
    slot: usize,
    generation: u32,
    peer_id: String,
    active: bool,
}

fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 6 {
        bail!(
            "usage: godot_voice_churn ENDPOINT_JSON EVENT_JSONL PEERS ACTIVE_SPEAKERS PHASE_SECONDS"
        );
    }
    let endpoint = parse_endpoint(&args[1])?;
    let event_path = &args[2];
    let peer_count = args[3].parse::<usize>().context("parse PEERS")?;
    let active_speakers = args[4].parse::<usize>().context("parse ACTIVE_SPEAKERS")?;
    let phase_seconds = args[5].parse::<f64>().context("parse PHASE_SECONDS")?;
    if peer_count == 0
        || active_speakers == 0
        || peer_count < active_speakers * 3
        || phase_seconds < 1.0
    {
        bail!("require PEERS >= 3 * ACTIVE_SPEAKERS > 0 and PHASE_SECONDS >= 1");
    }

    let mut events = BufWriter::new(
        File::create(event_path).with_context(|| format!("create event log {event_path}"))?,
    );
    let setup_start = Instant::now();
    let mut peers = Vec::with_capacity(peer_count);
    for slot in 0..peer_count {
        let peer = connect_peer(&endpoint, slot, 0)?;
        log_event(&mut events, "joined", &peer)?;
        peers.push(Some(peer));
    }
    // Bind replacement endpoints before the media clock starts. Constructing
    // seven Tokio/Iroh runtimes mid-talkspurt is artificial co-host contention;
    // the scenario should measure connection churn, not runtime construction.
    let mut prepared_replacements = Vec::with_capacity(active_speakers);
    for slot in group_slots(0, active_speakers) {
        prepared_replacements.push((slot, bind_service()?));
    }
    let setup_ms = setup_start.elapsed().as_secs_f64() * 1_000.0;

    thread::sleep(Duration::from_millis(100));
    let phase_frames = (phase_seconds / FRAME_DURATION.as_secs_f64()).round() as u64;
    let total_frames = phase_frames * 5 + DRAIN_FRAMES;
    let mut next_deadline = Instant::now();
    let mut sent_datagrams = 0_u64;
    let mut send_errors = 0_u64;
    let mut retired_peers = Vec::new();
    let (replacement_tx, replacement_rx) = mpsc::channel::<Result<Peer>>();

    for tick in 0..total_frames {
        for peer in replacement_rx.try_iter() {
            let mut peer = peer?;
            log_event(&mut events, "joined", &peer)?;
            if tick >= phase_frames * 3 && tick < phase_frames * 4 {
                peer.active = true;
                log_event(&mut events, "talker_on", &peer)?;
            }
            let slot = peer.slot;
            peers[slot] = Some(peer);
        }
        if tick == 0 {
            set_group_active(&mut peers, 0, active_speakers, true, &mut events)?;
        } else if tick == phase_frames {
            set_group_active(&mut peers, 0, active_speakers, false, &mut events)?;
            set_group_active(&mut peers, 1, active_speakers, true, &mut events)?;
        } else if tick == phase_frames * 2 {
            leave_group(
                &mut peers,
                0,
                active_speakers,
                "leave_requested",
                &mut events,
                &mut retired_peers,
            )?;
            // Joining a replacement can occasionally block for seconds inside
            // Iroh. Keep that control-plane work off the 20 ms media loop.
            for (slot, service) in prepared_replacements.drain(..) {
                let endpoint = endpoint.clone();
                let tx = replacement_tx.clone();
                thread::spawn(move || {
                    let result = connect_prepared_peer(&endpoint, service, slot, 1);
                    let _ = tx.send(result);
                });
            }
        } else if tick == phase_frames * 3 {
            set_group_active(&mut peers, 1, active_speakers, false, &mut events)?;
            set_group_active_if_present(&mut peers, 0, active_speakers, true, &mut events)?;
        } else if tick == phase_frames * 4 {
            if group_slots(0, active_speakers).any(|slot| peers[slot].is_none()) {
                bail!("replacement peers did not join within one phase");
            }
            set_group_active(&mut peers, 2, active_speakers, true, &mut events)?;
            leave_group(
                &mut peers,
                0,
                active_speakers,
                "leave_requested",
                &mut events,
                &mut retired_peers,
            )?;
        } else if tick == phase_frames * 5 {
            set_group_active(&mut peers, 2, active_speakers, false, &mut events)?;
        }

        let now = Instant::now();
        if let Some(remaining) = next_deadline.checked_duration_since(now) {
            thread::sleep(remaining);
        } else if now.saturating_duration_since(next_deadline) > FRAME_DURATION {
            // Connection setup and endpoint shutdown are synchronous. Never turn
            // that control-plane pause into a burst of catch-up voice packets.
            next_deadline = now;
        }

        for peer in peers.iter_mut().flatten() {
            if !peer.active && !peer.closing_talkspurt {
                peer.encoder.advance_dropped_frames(1);
                continue;
            }
            let frame = (0..FRAME_SAMPLES)
                .map(|sample| {
                    if peer.active {
                        let phase = (peer.phase_samples + sample as u64) as f32 / SAMPLE_RATE;
                        0.18 * (std::f32::consts::TAU * peer.frequency_hz * phase).sin()
                    } else {
                        0.0
                    }
                })
                .collect::<Vec<_>>();
            peer.phase_samples += FRAME_SAMPLES as u64;
            peer.encoder.push_pcm(&frame);
            if let Some(packet) = peer.encoder.poll_packet()? {
                let ended_talkspurt = packet.flags.contains(PacketFlags::END_OF_TALKSPURT);
                match peer
                    .service
                    .send_datagram(peer.remote, Bytes::from(packet.to_bytes()))
                {
                    Ok(()) => sent_datagrams += 1,
                    Err(_) => send_errors += 1,
                }
                if ended_talkspurt {
                    peer.closing_talkspurt = false;
                }
            }
        }
        next_deadline += FRAME_DURATION;
    }

    for peer in peers.iter_mut().flatten() {
        log_event(&mut events, "shutdown_requested", peer)?;
    }
    events.flush()?;
    let mut shutdown_tasks = Vec::new();
    retired_peers.extend(peers.into_iter().flatten());
    spawn_shutdowns(retired_peers, &mut shutdown_tasks);
    for task in shutdown_tasks {
        let _ = task.join();
    }

    println!(
        "{{\"peers\":{peer_count},\"active_speakers\":{active_speakers},\"phase_seconds\":{phase_seconds},\"setup_ms\":{setup_ms:.3},\"ticks\":{total_frames},\"sent_datagrams\":{sent_datagrams},\"send_errors\":{send_errors}}}"
    );
    Ok(())
}

fn connect_peer(endpoint: &EndpointAddr, slot: usize, generation: u32) -> Result<Peer> {
    connect_prepared_peer(endpoint, bind_service()?, slot, generation)
}

fn bind_service() -> Result<VoiceIrohService> {
    VoiceIrohService::bind(VoiceIrohConfig {
        bind_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        relay: false,
        ..Default::default()
    })
}

fn connect_prepared_peer(
    endpoint: &EndpointAddr,
    service: VoiceIrohService,
    slot: usize,
    generation: u32,
) -> Result<Peer> {
    let remote = service
        .connect(endpoint.clone())
        .with_context(|| format!("connect churn peer slot {slot} generation {generation}"))?;
    let peer = Peer {
        frequency_hz: 180.0 + slot as f32 * 23.0 + generation as f32 * 7.0,
        phase_samples: 0,
        encoder: VoiceEncoder::new(VoiceEncoderConfig {
            enable_dtx: true,
            ..Default::default()
        })?,
        service,
        remote,
        slot,
        generation,
        active: false,
        closing_talkspurt: false,
    };
    Ok(peer)
}

fn set_group_active(
    peers: &mut [Option<Peer>],
    group: usize,
    active_speakers: usize,
    active: bool,
    events: &mut BufWriter<File>,
) -> Result<()> {
    for slot in group_slots(group, active_speakers) {
        let peer = peers[slot]
            .as_mut()
            .with_context(|| format!("missing peer in slot {slot}"))?;
        if peer.active != active {
            peer.closing_talkspurt = !active;
            peer.active = active;
            log_event(
                events,
                if active { "talker_on" } else { "talker_off" },
                peer,
            )?;
        }
    }
    Ok(())
}

fn set_group_active_if_present(
    peers: &mut [Option<Peer>],
    group: usize,
    active_speakers: usize,
    active: bool,
    events: &mut BufWriter<File>,
) -> Result<()> {
    for slot in group_slots(group, active_speakers) {
        let Some(peer) = peers[slot].as_mut() else {
            continue;
        };
        if peer.active != active {
            peer.closing_talkspurt = !active;
            peer.active = active;
            log_event(
                events,
                if active { "talker_on" } else { "talker_off" },
                peer,
            )?;
        }
    }
    Ok(())
}

fn leave_group(
    peers: &mut [Option<Peer>],
    group: usize,
    active_speakers: usize,
    event: &str,
    events: &mut BufWriter<File>,
    retired_peers: &mut Vec<Peer>,
) -> Result<()> {
    let mut departing = Vec::with_capacity(active_speakers);
    for slot in group_slots(group, active_speakers) {
        if let Some(peer) = peers[slot].as_ref() {
            log_event(events, event, peer)?;
        }
        if let Some(peer) = peers[slot].take() {
            departing.push(peer);
        }
    }
    for peer in &departing {
        let _ = peer.service.disconnect(peer.remote);
    }
    retired_peers.extend(departing);
    Ok(())
}

fn spawn_shutdowns(peers: Vec<Peer>, tasks: &mut Vec<thread::JoinHandle<()>>) {
    // Each sidecar owns a runtime whose graceful endpoint close may take a few
    // hundred milliseconds. Closing a population serially makes the scenario
    // duration depend on N and can outlive the receiver gate.
    tasks.extend(
        peers
            .into_iter()
            .map(|peer| thread::spawn(move || drop(peer))),
    );
}

fn group_slots(group: usize, active_speakers: usize) -> std::ops::Range<usize> {
    let start = group * active_speakers;
    start..start + active_speakers
}

fn log_event(events: &mut BufWriter<File>, event: &str, peer: &Peer) -> Result<()> {
    serde_json::to_writer(
        &mut *events,
        &EventRecord {
            mono_usec: monotonic_time_us(),
            unix_usec: unix_time_us(),
            event,
            slot: peer.slot,
            generation: peer.generation,
            peer_id: peer.service.endpoint_id().to_string(),
            active: peer.active,
        },
    )?;
    events.write_all(b"\n")?;
    events.flush()?;
    Ok(())
}

fn monotonic_time_us() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` points to writable storage and CLOCK_MONOTONIC is shared
    // by the Rust load generator and Godot's Time.get_ticks_usec on Linux.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    if result != 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add(time.tv_nsec as u64 / 1_000)
}

fn unix_time_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn parse_endpoint(path: &str) -> Result<EndpointAddr> {
    let info: EndpointInfo =
        serde_json::from_slice(&fs::read(path).with_context(|| format!("read {path}"))?)?;
    let endpoint_id = EndpointId::from_str(&info.endpoint_id)?;
    let mut addrs = info
        .ip_addrs
        .into_iter()
        .map(|addr| addr.parse().map(TransportAddr::Ip))
        .collect::<Result<Vec<_>, _>>()?;
    for relay in info.relay_urls {
        addrs.push(TransportAddr::Relay(RelayUrl::from_str(&relay)?));
    }
    Ok(EndpointAddr::from_parts(endpoint_id, addrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talker_groups_are_disjoint_and_dense() {
        assert_eq!(
            group_slots(0, 7).collect::<Vec<_>>(),
            (0..7).collect::<Vec<_>>()
        );
        assert_eq!(
            group_slots(1, 7).collect::<Vec<_>>(),
            (7..14).collect::<Vec<_>>()
        );
        assert_eq!(
            group_slots(2, 7).collect::<Vec<_>>(),
            (14..21).collect::<Vec<_>>()
        );
    }
}

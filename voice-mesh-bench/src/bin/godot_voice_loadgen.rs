use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use godot_network_audio_iroh::{RemotePeer, VoiceIrohConfig, VoiceIrohService};
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use iroh_base::TransportAddr;
use serde::Deserialize;
use voice_core::{VoiceEncoder, VoiceEncoderConfig};

const FRAME_SAMPLES: usize = 960;
const SAMPLE_RATE: f32 = 48_000.0;
const FRAME_DURATION: Duration = Duration::from_millis(20);

#[derive(Deserialize)]
struct EndpointInfo {
    endpoint_id: String,
    ip_addrs: Vec<String>,
    relay_urls: Vec<String>,
}

struct Peer {
    service: VoiceIrohService,
    remote: RemotePeer,
    encoder: Option<VoiceEncoder>,
    phase_samples: u64,
    frequency_hz: f32,
}

fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 5 {
        bail!("usage: godot_voice_loadgen ENDPOINT_JSON PEERS ACTIVE_SPEAKERS SECONDS");
    }
    let endpoint = parse_endpoint(&args[1])?;
    let peer_count = args[2].parse::<usize>().context("parse PEERS")?;
    let active_speakers = args[3].parse::<usize>().context("parse ACTIVE_SPEAKERS")?;
    let seconds = args[4].parse::<f64>().context("parse SECONDS")?;
    if peer_count == 0 || active_speakers > peer_count || seconds <= 0.0 {
        bail!("require PEERS > 0, ACTIVE_SPEAKERS <= PEERS, and SECONDS > 0");
    }

    let setup_start = Instant::now();
    let mut peers = Vec::with_capacity(peer_count);
    for index in 0..peer_count {
        let service = VoiceIrohService::bind(VoiceIrohConfig {
            bind_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            relay: false,
            ..Default::default()
        })?;
        let remote = service
            .connect(endpoint.clone())
            .with_context(|| format!("connect load peer {index}"))?;
        let encoder = (index < active_speakers)
            .then(|| {
                VoiceEncoder::new(VoiceEncoderConfig {
                    enable_dtx: false,
                    ..Default::default()
                })
            })
            .transpose()?;
        peers.push(Peer {
            service,
            remote,
            encoder,
            phase_samples: 0,
            frequency_hz: 180.0 + index as f32 * 23.0,
        });
    }
    let setup_ms = setup_start.elapsed().as_secs_f64() * 1_000.0;

    // Give the Godot main thread one frame to construct players for every peer.
    thread::sleep(Duration::from_millis(100));
    let media_start = Instant::now();
    let media_end = media_start + Duration::from_secs_f64(seconds);
    let mut tick = 0_u64;
    let mut sent_datagrams = 0_u64;
    let mut send_errors = 0_u64;
    while Instant::now() < media_end {
        let deadline = media_start + FRAME_DURATION.mul_f64(tick as f64);
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        }
        for peer in &mut peers {
            let Some(encoder) = peer.encoder.as_mut() else {
                continue;
            };
            let frame = (0..FRAME_SAMPLES)
                .map(|sample| {
                    let phase = (peer.phase_samples + sample as u64) as f32 / SAMPLE_RATE;
                    0.18 * (std::f32::consts::TAU * peer.frequency_hz * phase).sin()
                })
                .collect::<Vec<_>>();
            peer.phase_samples += FRAME_SAMPLES as u64;
            encoder.push_pcm(&frame);
            if let Some(packet) = encoder.poll_packet()? {
                match peer
                    .service
                    .send_datagram(peer.remote, Bytes::from(packet.to_bytes()))
                {
                    Ok(()) => sent_datagrams += 1,
                    Err(_) => send_errors += 1,
                }
            }
        }
        tick += 1;
    }

    println!(
        "{{\"peers\":{peer_count},\"active_speakers\":{active_speakers},\"setup_ms\":{setup_ms:.3},\"ticks\":{tick},\"sent_datagrams\":{sent_datagrams},\"send_errors\":{send_errors}}}"
    );
    Ok(())
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

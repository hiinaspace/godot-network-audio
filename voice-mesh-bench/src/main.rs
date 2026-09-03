use std::{
    env, fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use iroh::{
    endpoint::{presets, Connection},
    Endpoint,
};
use serde::Serialize;
use tokio::{runtime::Builder, sync::mpsc, task::JoinHandle, time::MissedTickBehavior};
use voice_core::{PacketArrival, VoiceEncoder, VoiceEncoderConfig, VoicePacket, VoiceReceiver};

const ALPN: &[u8] = b"godot-network-audio/mesh-bench/0";
const SAMPLE_RATE: u32 = 48_000;
const ENCODE_FRAME_SAMPLES: usize = 960;
const PULL_FRAME_SAMPLES: usize = 480;
const ENVELOPE_HEADER_LEN: usize = 8;
const PLAYOUT_TICK: Duration = Duration::from_millis(10);
const SEND_TICK: Duration = Duration::from_millis(20);
const STARTUP_SETTLE: Duration = Duration::from_millis(100);
const DELIVERY_GRACE: Duration = Duration::from_millis(300);
const DEADLINE_LATE_US: u64 = 2_000;

#[derive(Debug, Clone)]
struct Config {
    participants: usize,
    talkers: usize,
    duration: Duration,
    dtx: bool,
    output: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            participants: 4,
            talkers: 1,
            duration: Duration::from_secs(5),
            dtx: true,
            output: None,
        }
    }
}

impl Config {
    fn parse() -> Result<Option<Self>> {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--participants" => {
                    config.participants = parse_next(&mut args, "--participants")?;
                }
                "--talkers" => config.talkers = parse_next(&mut args, "--talkers")?,
                "--seconds" => {
                    let seconds: f64 = parse_next(&mut args, "--seconds")?;
                    if !seconds.is_finite() || seconds < 0.5 {
                        bail!("--seconds must be finite and at least 0.5");
                    }
                    config.duration = Duration::from_secs_f64(seconds);
                }
                "--dtx" => {
                    let value = args.next().context("--dtx requires on or off")?;
                    config.dtx = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => bail!("--dtx must be on or off, got {value}"),
                    };
                }
                "--output" => {
                    config.output = Some(PathBuf::from(
                        args.next().context("--output requires a path")?,
                    ));
                }
                _ => bail!("unknown argument {arg}; use --help"),
            }
        }

        if !(2..=64).contains(&config.participants) {
            bail!("--participants must be between 2 and 64");
        }
        if config.talkers == 0 || config.talkers > config.participants {
            bail!("--talkers must be between 1 and the participant count");
        }
        Ok(Some(config))
    }
}

fn parse_next<T>(args: &mut impl Iterator<Item = String>, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = args
        .next()
        .with_context(|| format!("{name} requires a value"))?;
    raw.parse()
        .map_err(|err| anyhow::anyhow!("invalid value for {name}: {err}"))
}

#[derive(Debug)]
struct ReceivedDatagram {
    speaker: usize,
    bytes: Bytes,
    received_at_us: u64,
}

struct Mesh {
    endpoints: Vec<Endpoint>,
    connections: Vec<Vec<Option<Connection>>>,
    readers: Vec<JoinHandle<()>>,
}

impl Mesh {
    async fn close(self) {
        for endpoint in &self.endpoints {
            endpoint.close().await;
        }
        for reader in self.readers {
            reader.abort();
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ProcessUsage {
    cpu_seconds: f64,
    max_rss_kib: i64,
}

#[derive(Debug, Serialize)]
struct Metrics {
    schema_version: u32,
    topology: &'static str,
    participants: usize,
    talkers: usize,
    dtx: bool,
    requested_duration_seconds: f64,
    mesh_connections: usize,
    active_receiver_count: usize,
    setup_wall_ms: f64,
    setup_cpu_seconds: f64,
    media_wall_ms: f64,
    media_cpu_seconds: f64,
    media_cpu_percent_of_one_core: f64,
    max_rss_kib: i64,
    sender_ticks: u64,
    sender_callbacks: u64,
    sender_skipped_ticks: u64,
    encoded_packets: u64,
    sent_datagrams: u64,
    send_errors: u64,
    received_datagrams: u64,
    malformed_datagrams: u64,
    missing_datagrams: u64,
    sent_bytes: u64,
    received_bytes: u64,
    outbound_mbit_per_second: f64,
    latency_us_p50: u64,
    latency_us_p95: u64,
    latency_us_p99: u64,
    latency_us_max: u64,
    receive_queue_delay_us_p95: u64,
    receive_queue_delay_us_max: u64,
    playout_ticks: u64,
    playout_callbacks: u64,
    playout_skipped_ticks: u64,
    playout_deadline_misses: u64,
    playout_deadline_miss_percent: f64,
    playout_lateness_us_max: u64,
    neteq_concealed_samples: u64,
    neteq_concealed_percent: f64,
    neteq_receiver_errors: usize,
    neteq_max_current_buffer_ms: u32,
    neteq_max_target_delay_ms: u32,
}

#[derive(Debug, Default)]
struct RunCounters {
    received_datagrams: u64,
    malformed_datagrams: u64,
    received_bytes: u64,
    latencies_us: Vec<u64>,
    queue_delays_us: Vec<u64>,
    playout_ticks: u64,
    playout_callbacks: u64,
    playout_skipped_ticks: u64,
    playout_deadline_misses: u64,
    playout_lateness_us_max: u64,
}

#[derive(Debug, Default)]
struct SendCounters {
    sender_ticks: u64,
    sender_callbacks: u64,
    sender_skipped_ticks: u64,
    encoded_packets: u64,
    sent_datagrams: u64,
    send_errors: u64,
    sent_bytes: u64,
}

#[derive(Debug, Default)]
struct ListenerResult {
    counters: RunCounters,
    concealed_samples: u64,
    receiver_errors: usize,
    max_current_buffer_ms: u32,
    max_target_delay_ms: u32,
}

impl RunCounters {
    fn merge(&mut self, mut other: Self) {
        self.received_datagrams += other.received_datagrams;
        self.malformed_datagrams += other.malformed_datagrams;
        self.received_bytes += other.received_bytes;
        self.latencies_us.append(&mut other.latencies_us);
        self.queue_delays_us.append(&mut other.queue_delays_us);
        self.playout_ticks += other.playout_ticks;
        self.playout_callbacks += other.playout_callbacks;
        self.playout_skipped_ticks += other.playout_skipped_ticks;
        self.playout_deadline_misses += other.playout_deadline_misses;
        self.playout_lateness_us_max = self
            .playout_lateness_us_max
            .max(other.playout_lateness_us_max);
    }
}

fn main() -> Result<()> {
    let Some(config) = Config::parse()? else {
        print_help();
        return Ok(());
    };
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("voice-mesh-bench")
        .build()
        .context("build Tokio runtime")?;
    let metrics = runtime.block_on(run(config.clone()))?;
    let json = serde_json::to_string_pretty(&metrics)?;
    if let Some(path) = config.output {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }
        fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("write metrics to {}", path.display()))?;
    }
    println!("{json}");
    Ok(())
}

fn print_help() {
    println!(
        "voice-mesh-bench\n\
         \nUSAGE:\n\
         \x20 voice-mesh-bench [OPTIONS]\n\
         \nOPTIONS:\n\
         \x20 --participants N  Fully connected participants (2-64; default 4)\n\
         \x20 --talkers N       Scheduled talkers (default 1)\n\
         \x20 --seconds N       Media duration, at least 0.5 (default 5)\n\
         \x20 --dtx on|off      Opus discontinuous transmission (default on)\n\
         \x20 --output PATH     Also write pretty JSON metrics to PATH\n\
         \x20 -h, --help        Show this help"
    );
}

async fn run(config: Config) -> Result<Metrics> {
    let process_start = process_usage()?;
    let setup_start = Instant::now();
    let clock_start = setup_start;
    let mut receive_txs = Vec::with_capacity(config.participants);
    let mut receive_rxs = Vec::with_capacity(config.participants);
    for _ in 0..config.participants {
        let (tx, rx) = mpsc::unbounded_channel();
        receive_txs.push(tx);
        receive_rxs.push(rx);
    }
    let mesh = build_mesh(config.participants, clock_start, receive_txs).await?;

    let encoders = (0..config.talkers)
        .map(|_| {
            VoiceEncoder::new(VoiceEncoderConfig {
                enable_dtx: config.dtx,
                ..Default::default()
            })
        })
        .collect::<voice_core::Result<Vec<_>>>()?;
    let receiver_rows = (0..config.participants)
        .map(|listener| build_listener_receivers(listener, config.talkers))
        .collect::<Result<Vec<_>>>()?;
    let setup_wall = setup_start.elapsed();
    let after_setup = process_usage()?;

    tokio::time::sleep(STARTUP_SETTLE).await;
    let media_start = Instant::now() + Duration::from_millis(20);
    let media_end = media_start + config.duration;
    let listener_tasks = receive_rxs
        .into_iter()
        .zip(receiver_rows)
        .enumerate()
        .map(|(listener, (receive_rx, receivers))| {
            tokio::spawn(run_listener(
                listener,
                config.participants,
                receive_rx,
                receivers,
                media_start,
                config.duration,
                clock_start,
            ))
        })
        .collect::<Vec<_>>();
    let sender = tokio::spawn(send_media(
        config.participants,
        config.duration,
        mesh.connections.clone(),
        encoders,
        media_start,
        clock_start,
    ));
    tokio::time::sleep_until(tokio::time::Instant::from_std(media_end)).await;
    let media_wall = media_start.elapsed();
    let after_media = process_usage()?;
    let send_counters = sender.await.context("media sender task panicked")??;
    let mut counters = RunCounters::default();
    let mut concealed_samples = 0_u64;
    let mut receiver_errors = 0_usize;
    let mut max_current_buffer_ms = 0_u32;
    let mut max_target_delay_ms = 0_u32;
    for task in listener_tasks {
        let listener = task.await.context("listener task panicked")??;
        counters.merge(listener.counters);
        concealed_samples = concealed_samples.saturating_add(listener.concealed_samples);
        receiver_errors += listener.receiver_errors;
        max_current_buffer_ms = max_current_buffer_ms.max(listener.max_current_buffer_ms);
        max_target_delay_ms = max_target_delay_ms.max(listener.max_target_delay_ms);
    }

    let cpu_seconds = after_media.cpu_seconds - after_setup.cpu_seconds;
    let media_seconds = media_wall.as_secs_f64();
    let missing_datagrams = send_counters
        .sent_datagrams
        .saturating_sub(counters.received_datagrams);
    let metrics = Metrics {
        schema_version: 1,
        topology: "direct-full-mesh",
        participants: config.participants,
        talkers: config.talkers,
        dtx: config.dtx,
        requested_duration_seconds: config.duration.as_secs_f64(),
        mesh_connections: config.participants * (config.participants - 1) / 2,
        active_receiver_count: config.talkers * (config.participants - 1),
        setup_wall_ms: setup_wall.as_secs_f64() * 1_000.0,
        setup_cpu_seconds: after_setup.cpu_seconds - process_start.cpu_seconds,
        media_wall_ms: media_seconds * 1_000.0,
        media_cpu_seconds: cpu_seconds,
        media_cpu_percent_of_one_core: if media_seconds > 0.0 {
            cpu_seconds / media_seconds * 100.0
        } else {
            0.0
        },
        max_rss_kib: after_media.max_rss_kib,
        sender_ticks: send_counters.sender_ticks,
        sender_callbacks: send_counters.sender_callbacks,
        sender_skipped_ticks: send_counters.sender_skipped_ticks,
        encoded_packets: send_counters.encoded_packets,
        sent_datagrams: send_counters.sent_datagrams,
        send_errors: send_counters.send_errors,
        received_datagrams: counters.received_datagrams,
        malformed_datagrams: counters.malformed_datagrams,
        missing_datagrams,
        sent_bytes: send_counters.sent_bytes,
        received_bytes: counters.received_bytes,
        outbound_mbit_per_second: if media_seconds > 0.0 {
            send_counters.sent_bytes as f64 * 8.0 / media_seconds / 1_000_000.0
        } else {
            0.0
        },
        latency_us_p50: percentile(&mut counters.latencies_us, 50),
        latency_us_p95: percentile(&mut counters.latencies_us, 95),
        latency_us_p99: percentile(&mut counters.latencies_us, 99),
        latency_us_max: counters.latencies_us.iter().copied().max().unwrap_or(0),
        receive_queue_delay_us_p95: percentile(&mut counters.queue_delays_us, 95),
        receive_queue_delay_us_max: counters.queue_delays_us.iter().copied().max().unwrap_or(0),
        playout_ticks: counters.playout_ticks,
        playout_callbacks: counters.playout_callbacks,
        playout_skipped_ticks: counters.playout_skipped_ticks,
        playout_deadline_misses: counters.playout_deadline_misses,
        playout_deadline_miss_percent: if counters.playout_ticks > 0 {
            counters.playout_deadline_misses as f64 / counters.playout_ticks as f64 * 100.0
        } else {
            0.0
        },
        playout_lateness_us_max: counters.playout_lateness_us_max,
        neteq_concealed_samples: concealed_samples,
        neteq_concealed_percent: {
            let possible_samples = counters.playout_callbacks
                * config.talkers as u64
                * (config.participants - 1) as u64
                * PULL_FRAME_SAMPLES as u64;
            if possible_samples > 0 {
                concealed_samples as f64 / possible_samples as f64 * 100.0
            } else {
                0.0
            }
        },
        neteq_receiver_errors: receiver_errors,
        neteq_max_current_buffer_ms: max_current_buffer_ms,
        neteq_max_target_delay_ms: max_target_delay_ms,
    };

    mesh.close().await;
    Ok(metrics)
}

async fn build_mesh(
    participants: usize,
    clock_start: Instant,
    receive_txs: Vec<mpsc::UnboundedSender<ReceivedDatagram>>,
) -> Result<Mesh> {
    let mut endpoints = Vec::with_capacity(participants);
    for _ in 0..participants {
        let endpoint = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .alpns(vec![ALPN.to_vec()])
            .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .context("bind benchmark endpoint to loopback")?
            .bind()
            .await
            .context("start benchmark endpoint")?;
        endpoints.push(endpoint);
    }

    let mut connections = vec![vec![None; participants]; participants];
    for connector in 0..participants {
        for acceptor in (connector + 1)..participants {
            let accepting_endpoint = endpoints[acceptor].clone();
            let accepting = async move {
                let incoming = accepting_endpoint
                    .accept()
                    .await
                    .context("endpoint closed while building mesh")?;
                incoming.await.context("accept mesh connection")
            };
            let connecting_endpoint = endpoints[connector].clone();
            let accepting_addr = endpoints[acceptor].addr();
            let connecting = async move {
                connecting_endpoint
                    .connect(accepting_addr, ALPN)
                    .await
                    .context("connect mesh endpoint")
            };
            let (outbound, inbound) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::try_join!(connecting, accepting)
            })
            .await
            .context("timed out building mesh connection")??;
            connections[connector][acceptor] = Some(outbound);
            connections[acceptor][connector] = Some(inbound);
        }
    }

    let mut readers = Vec::with_capacity(participants * (participants - 1));
    for (listener, listener_connections) in connections.iter().enumerate() {
        for (speaker, connection) in listener_connections.iter().enumerate() {
            let Some(connection) = connection.clone() else {
                continue;
            };
            let tx = receive_txs[listener].clone();
            readers.push(tokio::spawn(async move {
                while let Ok(bytes) = connection.read_datagram().await {
                    let received_at_us = clock_start.elapsed().as_micros() as u64;
                    if tx
                        .send(ReceivedDatagram {
                            speaker,
                            bytes,
                            received_at_us,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
    }

    Ok(Mesh {
        endpoints,
        connections,
        readers,
    })
}

fn build_listener_receivers(listener: usize, talkers: usize) -> Result<Vec<Option<VoiceReceiver>>> {
    let mut receivers = Vec::with_capacity(talkers);
    for speaker in 0..talkers {
        if listener == speaker {
            receivers.push(None);
        } else {
            receivers.push(Some(VoiceReceiver::new(SAMPLE_RATE)?));
        }
    }
    Ok(receivers)
}

async fn run_listener(
    listener: usize,
    participants: usize,
    mut receive_rx: mpsc::UnboundedReceiver<ReceivedDatagram>,
    mut receivers: Vec<Option<VoiceReceiver>>,
    media_start: Instant,
    duration: Duration,
    clock_start: Instant,
) -> Result<ListenerResult> {
    let media_end = media_start + duration;
    // Independent clients do not share an audio clock. Stagger their callbacks
    // across one 10 ms frame so a single-pod simulation does not create an
    // artificial synchronized wake-up storm.
    let phase_nanos = PLAYOUT_TICK.as_nanos() * listener as u128 / participants as u128;
    let first_tick = media_start + Duration::from_nanos(phase_nanos as u64);
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::from_std(first_tick), PLAYOUT_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut previous_scheduled = None;
    let mut counters = RunCounters::default();

    while Instant::now() < media_end {
        let scheduled = ticker.tick().await;
        let now = tokio::time::Instant::now();
        let skipped_ticks = previous_scheduled
            .map(|previous| {
                scheduled
                    .saturating_duration_since(previous)
                    .as_nanos()
                    .saturating_div(PLAYOUT_TICK.as_nanos())
                    .saturating_sub(1) as u64
            })
            .unwrap_or(0);
        previous_scheduled = Some(scheduled);
        counters.playout_callbacks += 1;
        counters.playout_ticks += skipped_ticks + 1;
        counters.playout_skipped_ticks += skipped_ticks;
        counters.playout_deadline_misses += skipped_ticks;

        let lateness_us = now.saturating_duration_since(scheduled).as_micros() as u64;
        counters.playout_lateness_us_max = counters.playout_lateness_us_max.max(lateness_us);
        if lateness_us > DEADLINE_LATE_US {
            counters.playout_deadline_misses += 1;
        }

        drain_received(
            listener,
            &mut receive_rx,
            &mut receivers,
            clock_start,
            &mut counters,
        )?;
        pull_receivers(&mut receivers);
    }

    drain_received(
        listener,
        &mut receive_rx,
        &mut receivers,
        clock_start,
        &mut counters,
    )?;
    let mut result = ListenerResult {
        counters,
        ..Default::default()
    };
    for receiver in receivers.iter().flatten() {
        let stats = receiver.stats();
        result.concealed_samples = result
            .concealed_samples
            .saturating_add(stats.concealed_samples);
        result.receiver_errors += usize::from(stats.sticky_error.is_some());
        result.max_current_buffer_ms = result
            .max_current_buffer_ms
            .max(stats.current_buffer_size_ms);
        result.max_target_delay_ms = result.max_target_delay_ms.max(stats.target_delay_ms);
    }

    let delivery_deadline = Instant::now() + DELIVERY_GRACE;
    while Instant::now() < delivery_deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
        drain_received(
            listener,
            &mut receive_rx,
            &mut receivers,
            clock_start,
            &mut result.counters,
        )?;
    }
    Ok(result)
}

async fn send_media(
    participants: usize,
    duration: Duration,
    connections: Vec<Vec<Option<Connection>>>,
    mut encoders: Vec<VoiceEncoder>,
    media_start: Instant,
    clock_start: Instant,
) -> Result<SendCounters> {
    let media_end = media_start + duration;
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::from_std(media_start), SEND_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut previous_scheduled = None;
    let mut frame_index = 0_u64;
    let mut counters = SendCounters::default();

    while Instant::now() < media_end {
        let scheduled = ticker.tick().await;
        let skipped_ticks = previous_scheduled
            .map(|previous| {
                scheduled
                    .saturating_duration_since(previous)
                    .as_nanos()
                    .saturating_div(SEND_TICK.as_nanos())
                    .saturating_sub(1) as u64
            })
            .unwrap_or(0);
        previous_scheduled = Some(scheduled);
        counters.sender_callbacks += 1;
        counters.sender_ticks += skipped_ticks + 1;
        counters.sender_skipped_ticks += skipped_ticks;
        frame_index += skipped_ticks;
        encode_and_send(
            participants,
            &connections,
            &mut encoders,
            frame_index,
            clock_start,
            &mut counters,
        )?;
        frame_index += 1;
    }
    Ok(counters)
}

fn encode_and_send(
    participants: usize,
    connections: &[Vec<Option<Connection>>],
    encoders: &mut [VoiceEncoder],
    frame_index: u64,
    clock_start: Instant,
    counters: &mut SendCounters,
) -> Result<()> {
    for (speaker, encoder) in encoders.iter_mut().enumerate() {
        let active = talker_active(frame_index, speaker);
        let pcm = speech_frame(frame_index, speaker, active);
        encoder.push_pcm(&pcm);
        let Some(packet) = encoder.poll_packet()? else {
            continue;
        };
        counters.encoded_packets += 1;

        let sent_at_us = clock_start.elapsed().as_micros() as u64;
        let mut wire = Vec::with_capacity(ENVELOPE_HEADER_LEN + VoicePacket::HEADER_LEN + 512);
        wire.extend_from_slice(&sent_at_us.to_be_bytes());
        packet.encode_to_bytes(&mut wire);
        let wire = Bytes::from(wire);
        for (listener, connection) in connections[speaker].iter().enumerate().take(participants) {
            if listener == speaker {
                continue;
            }
            let connection = connection
                .as_ref()
                .context("missing full-mesh connection")?;
            match connection.send_datagram(wire.clone()) {
                Ok(()) => {
                    counters.sent_datagrams += 1;
                    counters.sent_bytes += wire.len() as u64;
                }
                Err(_) => counters.send_errors += 1,
            }
        }
    }
    Ok(())
}

fn drain_received(
    listener: usize,
    receive_rx: &mut mpsc::UnboundedReceiver<ReceivedDatagram>,
    receivers: &mut [Option<VoiceReceiver>],
    clock_start: Instant,
    counters: &mut RunCounters,
) -> Result<()> {
    while let Ok(datagram) = receive_rx.try_recv() {
        counters.received_datagrams += 1;
        counters.received_bytes += datagram.bytes.len() as u64;
        let processed_at_us = clock_start.elapsed().as_micros() as u64;
        counters
            .queue_delays_us
            .push(processed_at_us.saturating_sub(datagram.received_at_us));

        if datagram.bytes.len() < ENVELOPE_HEADER_LEN {
            counters.malformed_datagrams += 1;
            continue;
        }
        let sent_at_us = u64::from_be_bytes(
            datagram.bytes[..ENVELOPE_HEADER_LEN]
                .try_into()
                .expect("checked envelope length"),
        );
        counters
            .latencies_us
            .push(datagram.received_at_us.saturating_sub(sent_at_us));
        let packet = match VoicePacket::decode_from_bytes(&datagram.bytes[ENVELOPE_HEADER_LEN..]) {
            Ok(packet) => packet,
            Err(_) => {
                counters.malformed_datagrams += 1;
                continue;
            }
        };
        let Some(receiver) = receivers.get_mut(datagram.speaker).and_then(Option::as_mut) else {
            if listener != datagram.speaker {
                counters.malformed_datagrams += 1;
            }
            continue;
        };
        receiver.push_packet_with_now_mono(
            packet,
            PacketArrival {
                received_at_mono_us: datagram.received_at_us,
            },
            processed_at_us,
        )?;
    }
    Ok(())
}

fn pull_receivers(receivers: &mut [Option<VoiceReceiver>]) {
    let mut frame = [0.0_f32; PULL_FRAME_SAMPLES];
    for receiver in receivers.iter_mut().flatten() {
        receiver.pull_frame(&mut frame);
    }
}

fn talker_active(frame_index: u64, speaker: usize) -> bool {
    const CYCLE_FRAMES: u64 = 150;
    const ACTIVE_FRAMES: u64 = 110;
    let offset = speaker as u64 * 37;
    (frame_index + offset) % CYCLE_FRAMES < ACTIVE_FRAMES
}

fn speech_frame(frame_index: u64, speaker: usize, active: bool) -> [f32; ENCODE_FRAME_SAMPLES] {
    let mut frame = [0.0_f32; ENCODE_FRAME_SAMPLES];
    if !active {
        return frame;
    }
    let base_hz = 150.0 + speaker as f32 * 29.0;
    let sample_offset = frame_index as usize * ENCODE_FRAME_SAMPLES;
    for (i, sample) in frame.iter_mut().enumerate() {
        let t = (sample_offset + i) as f32 / SAMPLE_RATE as f32;
        let syllable = ((2.0 * std::f32::consts::PI * 3.1 * t).sin() * 0.5 + 0.5).powf(1.4);
        let voiced = (2.0 * std::f32::consts::PI * base_hz * t).sin()
            + 0.30 * (2.0 * std::f32::consts::PI * base_hz * 3.0 * t).sin();
        *sample = 0.08 * syllable * voiced;
    }
    frame
}

fn percentile(values: &mut [u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = (values.len() - 1) * percentile / 100;
    values[index]
}

fn process_usage() -> Result<ProcessUsage> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the provided rusage on success, and the pointer is valid.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("read process resource usage");
    }
    // SAFETY: getrusage returned success, so the structure has been initialized.
    let usage = unsafe { usage.assume_init() };
    let user_seconds = usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1_000_000.0;
    let system_seconds = usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1_000_000.0;
    Ok(ProcessUsage {
        cpu_seconds: user_seconds + system_seconds,
        max_rss_kib: usage.ru_maxrss,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talker_schedule_contains_speech_and_silence() {
        for speaker in 0..4 {
            assert!((0..150).any(|frame| talker_active(frame, speaker)));
            assert!((0..150).any(|frame| !talker_active(frame, speaker)));
        }
    }

    #[test]
    fn percentile_handles_edges() {
        assert_eq!(percentile(&mut [], 95), 0);
        assert_eq!(percentile(&mut [9], 95), 9);
        assert_eq!(percentile(&mut [5, 1, 3, 2, 4], 50), 3);
        assert_eq!(percentile(&mut [5, 1, 3, 2, 4], 95), 4);
    }
}

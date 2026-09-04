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
use voice_core::{
    PacketArrival, PacketFlags, VoiceEncoder, VoiceEncoderConfig, VoicePacket, VoiceReceiver,
};

const ALPN: &[u8] = b"godot-network-audio/mesh-bench/0";
const SAMPLE_RATE: u32 = 48_000;
const ENCODE_FRAME_SAMPLES: usize = 960;
const PULL_FRAME_SAMPLES: usize = 480;
const ENVELOPE_HEADER_LEN: usize = 16;
const PLAYOUT_TICK: Duration = Duration::from_millis(10);
const SEND_TICK: Duration = Duration::from_millis(20);
const STARTUP_SETTLE: Duration = Duration::from_millis(100);
const DELIVERY_GRACE: Duration = Duration::from_millis(300);
const DEADLINE_LATE_US: u64 = 2_000;
const GAME_TURN_FRAMES: u64 = 75;
const GAME_TALK_FRAMES: u64 = 60;
const GAME_OVERLAP_FRAMES: u64 = 5;
const GAME_INTEREST_EPOCH_FRAMES: u64 = 150;
const STRESS_CYCLE_FRAMES: u64 = 600;
const STRESS_WINDOW_START_FRAMES: u64 = 200;
const STRESS_WINDOW_END_FRAMES: u64 = 400;
const CROWD_BURST_START_FRAMES: u64 = 250;
const CROWD_BURST_END_FRAMES: u64 = 300;
const BOUNDARY_OSCILLATION_FRAMES: u64 = 5;
const NON_SILENT_RMS: f32 = 0.000_1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Baseline,
    GameInterest,
}

impl Scenario {
    fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::GameInterest => "game-interest",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    SenderFiltered,
    BroadcastDiscard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverPolicy {
    Retire,
    Pool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterestProfile {
    Rotating,
    CrowdBurst,
    GroupMerge,
    BoundaryOscillation,
}

impl InterestProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rotating => "rotating",
            Self::CrowdBurst => "crowd-burst",
            Self::GroupMerge => "group-merge",
            Self::BoundaryOscillation => "boundary-oscillation",
        }
    }
}

impl ReceiverPolicy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Retire => "retire",
            Self::Pool => "pool",
        }
    }
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Self::SenderFiltered => "sender-filtered",
            Self::BroadcastDiscard => "broadcast-discard",
        }
    }
}

#[derive(Debug, Clone)]
struct Config {
    participants: usize,
    talkers: usize,
    duration: Duration,
    dtx: bool,
    scenario: Scenario,
    delivery: Delivery,
    receiver_policy: ReceiverPolicy,
    interest_profile: InterestProfile,
    interest_listeners: usize,
    seed: u64,
    runtime_workers: usize,
    output: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            participants: 4,
            talkers: 1,
            duration: Duration::from_secs(5),
            dtx: true,
            scenario: Scenario::Baseline,
            delivery: Delivery::SenderFiltered,
            receiver_policy: ReceiverPolicy::Retire,
            interest_profile: InterestProfile::Rotating,
            interest_listeners: 7,
            seed: 1,
            runtime_workers: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
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
                "--scenario" => {
                    let value = args.next().context("--scenario requires a value")?;
                    config.scenario = match value.as_str() {
                        "baseline" => Scenario::Baseline,
                        "game-interest" => Scenario::GameInterest,
                        _ => bail!("--scenario must be baseline or game-interest, got {value}"),
                    };
                }
                "--delivery" => {
                    let value = args.next().context("--delivery requires a value")?;
                    config.delivery = match value.as_str() {
                        "sender-filtered" => Delivery::SenderFiltered,
                        "broadcast-discard" => Delivery::BroadcastDiscard,
                        _ => bail!(
                            "--delivery must be sender-filtered or broadcast-discard, got {value}"
                        ),
                    };
                }
                "--receiver-policy" => {
                    let value = args.next().context("--receiver-policy requires a value")?;
                    config.receiver_policy = match value.as_str() {
                        "retire" => ReceiverPolicy::Retire,
                        "pool" => ReceiverPolicy::Pool,
                        _ => bail!("--receiver-policy must be retire or pool, got {value}"),
                    };
                }
                "--interest-profile" => {
                    let value = args.next().context("--interest-profile requires a value")?;
                    config.interest_profile = match value.as_str() {
                        "rotating" => InterestProfile::Rotating,
                        "crowd-burst" => InterestProfile::CrowdBurst,
                        "group-merge" => InterestProfile::GroupMerge,
                        "boundary-oscillation" => InterestProfile::BoundaryOscillation,
                        _ => bail!(
                            "--interest-profile must be rotating, crowd-burst, group-merge, or boundary-oscillation, got {value}"
                        ),
                    };
                }
                "--interest-listeners" => {
                    config.interest_listeners = parse_next(&mut args, "--interest-listeners")?;
                }
                "--seed" => config.seed = parse_next(&mut args, "--seed")?,
                "--runtime-workers" => {
                    config.runtime_workers = parse_next(&mut args, "--runtime-workers")?;
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
        if !(1..=128).contains(&config.runtime_workers) {
            bail!("--runtime-workers must be between 1 and 128");
        }
        if config.scenario == Scenario::GameInterest {
            if config.interest_listeners == 0 || config.interest_listeners >= config.participants {
                bail!("--interest-listeners must be between 1 and participants - 1");
            }
            if config.talkers > 8 {
                bail!("game-interest supports at most 8 simultaneous conversation slots");
            }
            if config.interest_profile == InterestProfile::BoundaryOscillation
                && config.interest_listeners * 2 > config.participants - 1
            {
                bail!(
                    "boundary-oscillation requires two disjoint listener sets; use --interest-listeners no greater than (participants - 1) / 2"
                );
            }
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
struct RssSample {
    elapsed_seconds: f64,
    current_rss_kib: i64,
}

#[derive(Debug, Serialize)]
struct Metrics {
    schema_version: u32,
    topology: &'static str,
    scenario: &'static str,
    delivery: &'static str,
    receiver_policy: &'static str,
    interest_profile: &'static str,
    seed: u64,
    runtime_worker_threads: usize,
    participants: usize,
    talkers: usize,
    interest_listeners: usize,
    max_interest_listeners: usize,
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
    current_rss_kib_after_setup: i64,
    current_rss_kib_after_media: i64,
    rss_samples: Vec<RssSample>,
    sender_ticks: u64,
    sender_callbacks: u64,
    sender_skipped_ticks: u64,
    stress_events: u64,
    stress_sender_ticks: u64,
    stress_sender_skipped_ticks: u64,
    stress_sent_datagrams: u64,
    stress_fanout_span_us_p95: u64,
    sender_callback_work_us_p95: u64,
    stress_sender_callback_work_us_p95: u64,
    fanout_span_us_p50: u64,
    fanout_span_us_p95: u64,
    fanout_span_us_max: u64,
    encoded_packets: u64,
    sent_datagrams: u64,
    send_errors: u64,
    received_datagrams: u64,
    accepted_datagrams: u64,
    outside_interest_datagrams: u64,
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
    interest_entry_to_first_media_us_p50: u64,
    interest_entry_to_first_media_us_p95: u64,
    interest_entry_to_first_media_us_max: u64,
    interest_entry_events: usize,
    talkspurt_start_to_audio_us_p50: u64,
    talkspurt_start_to_audio_us_p95: u64,
    talkspurt_start_to_audio_us_max: u64,
    talkspurt_audio_events: usize,
    playout_ticks: u64,
    playout_callbacks: u64,
    playout_skipped_ticks: u64,
    playout_deadline_misses: u64,
    playout_deadline_miss_percent: f64,
    stress_playout_ticks: u64,
    stress_playout_deadline_misses: u64,
    stress_playout_deadline_miss_percent: f64,
    nonstress_playout_deadline_miss_percent: f64,
    stress_receive_queue_delay_us_p95: u64,
    listener_callback_work_us_p95: u64,
    stress_listener_callback_work_us_p95: u64,
    receive_drain_work_us_p95: u64,
    stress_receive_drain_work_us_p95: u64,
    playout_pull_work_us_p95: u64,
    stress_playout_pull_work_us_p95: u64,
    playout_lateness_us_max: u64,
    neteq_concealed_samples: u64,
    neteq_concealed_percent: f64,
    neteq_receiver_errors: usize,
    neteq_max_current_buffer_ms: u32,
    neteq_max_target_delay_ms: u32,
    receiver_creations: u64,
    receiver_reuses: u64,
    receiver_retirements: u64,
    max_concurrent_receivers: usize,
    max_receiver_pool: usize,
    participants_metrics: Vec<ParticipantMetrics>,
}

#[derive(Debug, Serialize)]
struct ParticipantMetrics {
    participant: usize,
    received_datagrams: u64,
    accepted_datagrams: u64,
    outside_interest_datagrams: u64,
    active_receiver_count: usize,
    receiver_creations: u64,
    receiver_reuses: u64,
    receiver_retirements: u64,
    max_concurrent_receivers: usize,
    max_receiver_pool: usize,
    receive_queue_delay_us_p95: u64,
    playout_skipped_ticks: u64,
    playout_deadline_miss_percent: f64,
    stress_playout_deadline_miss_percent: f64,
    playout_lateness_us_max: u64,
    interest_entry_to_first_media_us_p95: u64,
    interest_entry_events: usize,
    talkspurt_start_to_audio_us_p95: u64,
    talkspurt_audio_events: usize,
    neteq_concealed_samples: u64,
    neteq_receiver_errors: usize,
}

#[derive(Debug, Default)]
struct RunCounters {
    received_datagrams: u64,
    accepted_datagrams: u64,
    outside_interest_datagrams: u64,
    malformed_datagrams: u64,
    received_bytes: u64,
    latencies_us: Vec<u64>,
    queue_delays_us: Vec<u64>,
    stress_queue_delays_us: Vec<u64>,
    listener_callback_work_us: Vec<u64>,
    stress_listener_callback_work_us: Vec<u64>,
    receive_drain_work_us: Vec<u64>,
    stress_receive_drain_work_us: Vec<u64>,
    playout_pull_work_us: Vec<u64>,
    stress_playout_pull_work_us: Vec<u64>,
    interest_entry_to_first_media_us: Vec<u64>,
    talkspurt_start_to_audio_us: Vec<u64>,
    receiver_pull_samples: u64,
    playout_ticks: u64,
    playout_callbacks: u64,
    playout_skipped_ticks: u64,
    playout_deadline_misses: u64,
    stress_playout_ticks: u64,
    stress_playout_deadline_misses: u64,
    playout_lateness_us_max: u64,
}

#[derive(Debug, Default)]
struct SendCounters {
    sender_ticks: u64,
    sender_callbacks: u64,
    sender_skipped_ticks: u64,
    stress_events: u64,
    stress_sender_ticks: u64,
    stress_sender_skipped_ticks: u64,
    stress_sent_datagrams: u64,
    encoded_packets: u64,
    sent_datagrams: u64,
    send_errors: u64,
    sent_bytes: u64,
    fanout_spans_us: Vec<u64>,
    stress_fanout_spans_us: Vec<u64>,
    callback_work_us: Vec<u64>,
    stress_callback_work_us: Vec<u64>,
}

#[derive(Debug, Default)]
struct ListenerResult {
    participant: usize,
    counters: RunCounters,
    concealed_samples: u64,
    receiver_errors: usize,
    max_current_buffer_ms: u32,
    max_target_delay_ms: u32,
    active_receiver_count: usize,
    receiver_creations: u64,
    receiver_reuses: u64,
    receiver_retirements: u64,
    max_concurrent_receivers: usize,
    max_receiver_pool: usize,
}

struct ReceiverSlot {
    receiver: VoiceReceiver,
    pending_talkspurt_start_us: Option<u64>,
    reported_concealed_samples: u64,
    reported_error: bool,
}

#[derive(Debug, Default)]
struct ReceiverTotals {
    concealed_samples: u64,
    receiver_errors: usize,
    max_current_buffer_ms: u32,
    max_target_delay_ms: u32,
}

impl RunCounters {
    fn merge(&mut self, mut other: Self) {
        self.received_datagrams += other.received_datagrams;
        self.accepted_datagrams += other.accepted_datagrams;
        self.outside_interest_datagrams += other.outside_interest_datagrams;
        self.malformed_datagrams += other.malformed_datagrams;
        self.received_bytes += other.received_bytes;
        self.latencies_us.append(&mut other.latencies_us);
        self.queue_delays_us.append(&mut other.queue_delays_us);
        self.stress_queue_delays_us
            .append(&mut other.stress_queue_delays_us);
        self.listener_callback_work_us
            .append(&mut other.listener_callback_work_us);
        self.stress_listener_callback_work_us
            .append(&mut other.stress_listener_callback_work_us);
        self.receive_drain_work_us
            .append(&mut other.receive_drain_work_us);
        self.stress_receive_drain_work_us
            .append(&mut other.stress_receive_drain_work_us);
        self.playout_pull_work_us
            .append(&mut other.playout_pull_work_us);
        self.stress_playout_pull_work_us
            .append(&mut other.stress_playout_pull_work_us);
        self.interest_entry_to_first_media_us
            .append(&mut other.interest_entry_to_first_media_us);
        self.talkspurt_start_to_audio_us
            .append(&mut other.talkspurt_start_to_audio_us);
        self.receiver_pull_samples += other.receiver_pull_samples;
        self.playout_ticks += other.playout_ticks;
        self.playout_callbacks += other.playout_callbacks;
        self.playout_skipped_ticks += other.playout_skipped_ticks;
        self.playout_deadline_misses += other.playout_deadline_misses;
        self.stress_playout_ticks += other.stress_playout_ticks;
        self.stress_playout_deadline_misses += other.stress_playout_deadline_misses;
        self.playout_lateness_us_max = self
            .playout_lateness_us_max
            .max(other.playout_lateness_us_max);
    }
}

impl SendCounters {
    fn merge(&mut self, mut other: Self) {
        self.sender_ticks += other.sender_ticks;
        self.sender_callbacks += other.sender_callbacks;
        self.sender_skipped_ticks += other.sender_skipped_ticks;
        self.stress_events = self.stress_events.max(other.stress_events);
        self.stress_sender_ticks += other.stress_sender_ticks;
        self.stress_sender_skipped_ticks += other.stress_sender_skipped_ticks;
        self.stress_sent_datagrams += other.stress_sent_datagrams;
        self.encoded_packets += other.encoded_packets;
        self.sent_datagrams += other.sent_datagrams;
        self.send_errors += other.send_errors;
        self.sent_bytes += other.sent_bytes;
        self.fanout_spans_us.append(&mut other.fanout_spans_us);
        self.stress_fanout_spans_us
            .append(&mut other.stress_fanout_spans_us);
        self.callback_work_us.append(&mut other.callback_work_us);
        self.stress_callback_work_us
            .append(&mut other.stress_callback_work_us);
    }
}

fn main() -> Result<()> {
    let Some(config) = Config::parse()? else {
        print_help();
        return Ok(());
    };
    let runtime = Builder::new_multi_thread()
        .worker_threads(config.runtime_workers)
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
         \x20 --scenario NAME   baseline or game-interest (default baseline)\n\
         \x20 --delivery NAME   sender-filtered or broadcast-discard\n\
         \x20 --receiver-policy NAME  retire or pool (default retire)\n\
         \x20 --interest-profile NAME rotating, crowd-burst, group-merge, or boundary-oscillation\n\
         \x20 --interest-listeners N  Interested listeners per game talker (default 7)\n\
         \x20 --seed N          Deterministic game schedule seed (default 1)\n\
         \x20 --runtime-workers N  Tokio worker threads (default available CPUs)\n\
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

    let encoder_count = match config.scenario {
        Scenario::Baseline => config.talkers,
        Scenario::GameInterest => config.participants,
    };
    let encoders = (0..encoder_count)
        .map(|_| {
            VoiceEncoder::new(VoiceEncoderConfig {
                enable_dtx: config.dtx,
                ..Default::default()
            })
        })
        .collect::<voice_core::Result<Vec<_>>>()?;
    let receiver_rows = (0..config.participants)
        .map(|listener| build_listener_receivers(listener, &config, encoder_count))
        .collect::<Result<Vec<_>>>()?;
    let setup_wall = setup_start.elapsed();
    let after_setup = process_usage()?;
    let current_rss_kib_after_setup = current_rss_kib()?;

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
                config.clone(),
            ))
        })
        .collect::<Vec<_>>();
    let sender_count = encoders.len();
    let sender_tasks = encoders
        .into_iter()
        .enumerate()
        .map(|(speaker, encoder)| {
            tokio::spawn(send_media(
                speaker,
                sender_count,
                config.duration,
                mesh.connections[speaker].clone(),
                encoder,
                media_start,
                clock_start,
                config.clone(),
            ))
        })
        .collect::<Vec<_>>();
    let mut rss_samples = Vec::new();
    let mut next_rss_sample = media_start;
    while next_rss_sample < media_end {
        tokio::time::sleep_until(tokio::time::Instant::from_std(next_rss_sample)).await;
        rss_samples.push(RssSample {
            elapsed_seconds: media_start.elapsed().as_secs_f64(),
            current_rss_kib: current_rss_kib()?,
        });
        next_rss_sample += Duration::from_secs(10);
    }
    tokio::time::sleep_until(tokio::time::Instant::from_std(media_end)).await;
    let current_rss_kib_after_media = current_rss_kib()?;
    rss_samples.push(RssSample {
        elapsed_seconds: media_start.elapsed().as_secs_f64(),
        current_rss_kib: current_rss_kib_after_media,
    });
    let media_wall = media_start.elapsed();
    let after_media = process_usage()?;
    let mut send_counters = SendCounters::default();
    for task in sender_tasks {
        send_counters.merge(task.await.context("media sender task panicked")??);
    }
    let mut counters = RunCounters::default();
    let mut concealed_samples = 0_u64;
    let mut receiver_errors = 0_usize;
    let mut max_current_buffer_ms = 0_u32;
    let mut max_target_delay_ms = 0_u32;
    let mut active_receiver_count = 0_usize;
    let mut receiver_creations = 0_u64;
    let mut receiver_reuses = 0_u64;
    let mut receiver_retirements = 0_u64;
    let mut max_concurrent_receivers = 0_usize;
    let mut max_receiver_pool = 0_usize;
    let mut participants_metrics = Vec::with_capacity(config.participants);
    for task in listener_tasks {
        let mut listener = task.await.context("listener task panicked")??;
        participants_metrics.push(participant_metrics(&mut listener));
        active_receiver_count += listener.active_receiver_count;
        receiver_creations += listener.receiver_creations;
        receiver_reuses += listener.receiver_reuses;
        receiver_retirements += listener.receiver_retirements;
        max_concurrent_receivers = max_concurrent_receivers.max(listener.max_concurrent_receivers);
        max_receiver_pool = max_receiver_pool.max(listener.max_receiver_pool);
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
        schema_version: 3,
        topology: "direct-full-mesh",
        scenario: config.scenario.as_str(),
        delivery: match config.scenario {
            Scenario::Baseline => "full-broadcast",
            Scenario::GameInterest => config.delivery.as_str(),
        },
        receiver_policy: config.receiver_policy.as_str(),
        interest_profile: match config.scenario {
            Scenario::Baseline => "none",
            Scenario::GameInterest => config.interest_profile.as_str(),
        },
        seed: config.seed,
        runtime_worker_threads: config.runtime_workers,
        participants: config.participants,
        talkers: config.talkers,
        interest_listeners: match config.scenario {
            Scenario::Baseline => config.participants - 1,
            Scenario::GameInterest => config.interest_listeners,
        },
        max_interest_listeners: match config.scenario {
            Scenario::Baseline => config.participants - 1,
            Scenario::GameInterest => max_interest_listeners(&config),
        },
        dtx: config.dtx,
        requested_duration_seconds: config.duration.as_secs_f64(),
        mesh_connections: config.participants * (config.participants - 1) / 2,
        active_receiver_count,
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
        current_rss_kib_after_setup,
        current_rss_kib_after_media,
        rss_samples,
        sender_ticks: send_counters.sender_ticks,
        sender_callbacks: send_counters.sender_callbacks,
        sender_skipped_ticks: send_counters.sender_skipped_ticks,
        stress_events: send_counters.stress_events,
        stress_sender_ticks: send_counters.stress_sender_ticks,
        stress_sender_skipped_ticks: send_counters.stress_sender_skipped_ticks,
        stress_sent_datagrams: send_counters.stress_sent_datagrams,
        stress_fanout_span_us_p95: percentile(&mut send_counters.stress_fanout_spans_us, 95),
        sender_callback_work_us_p95: percentile(&mut send_counters.callback_work_us, 95),
        stress_sender_callback_work_us_p95: percentile(
            &mut send_counters.stress_callback_work_us,
            95,
        ),
        fanout_span_us_p50: percentile(&mut send_counters.fanout_spans_us, 50),
        fanout_span_us_p95: percentile(&mut send_counters.fanout_spans_us, 95),
        fanout_span_us_max: send_counters
            .fanout_spans_us
            .iter()
            .copied()
            .max()
            .unwrap_or(0),
        encoded_packets: send_counters.encoded_packets,
        sent_datagrams: send_counters.sent_datagrams,
        send_errors: send_counters.send_errors,
        received_datagrams: counters.received_datagrams,
        accepted_datagrams: counters.accepted_datagrams,
        outside_interest_datagrams: counters.outside_interest_datagrams,
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
        interest_entry_to_first_media_us_p50: percentile(
            &mut counters.interest_entry_to_first_media_us,
            50,
        ),
        interest_entry_to_first_media_us_p95: percentile(
            &mut counters.interest_entry_to_first_media_us,
            95,
        ),
        interest_entry_to_first_media_us_max: counters
            .interest_entry_to_first_media_us
            .iter()
            .copied()
            .max()
            .unwrap_or(0),
        interest_entry_events: counters.interest_entry_to_first_media_us.len(),
        talkspurt_start_to_audio_us_p50: percentile(&mut counters.talkspurt_start_to_audio_us, 50),
        talkspurt_start_to_audio_us_p95: percentile(&mut counters.talkspurt_start_to_audio_us, 95),
        talkspurt_start_to_audio_us_max: counters
            .talkspurt_start_to_audio_us
            .iter()
            .copied()
            .max()
            .unwrap_or(0),
        talkspurt_audio_events: counters.talkspurt_start_to_audio_us.len(),
        playout_ticks: counters.playout_ticks,
        playout_callbacks: counters.playout_callbacks,
        playout_skipped_ticks: counters.playout_skipped_ticks,
        playout_deadline_misses: counters.playout_deadline_misses,
        playout_deadline_miss_percent: if counters.playout_ticks > 0 {
            counters.playout_deadline_misses as f64 / counters.playout_ticks as f64 * 100.0
        } else {
            0.0
        },
        stress_playout_ticks: counters.stress_playout_ticks,
        stress_playout_deadline_misses: counters.stress_playout_deadline_misses,
        stress_playout_deadline_miss_percent: percent(
            counters.stress_playout_deadline_misses,
            counters.stress_playout_ticks,
        ),
        nonstress_playout_deadline_miss_percent: percent(
            counters
                .playout_deadline_misses
                .saturating_sub(counters.stress_playout_deadline_misses),
            counters
                .playout_ticks
                .saturating_sub(counters.stress_playout_ticks),
        ),
        stress_receive_queue_delay_us_p95: percentile(&mut counters.stress_queue_delays_us, 95),
        listener_callback_work_us_p95: percentile(&mut counters.listener_callback_work_us, 95),
        stress_listener_callback_work_us_p95: percentile(
            &mut counters.stress_listener_callback_work_us,
            95,
        ),
        receive_drain_work_us_p95: percentile(&mut counters.receive_drain_work_us, 95),
        stress_receive_drain_work_us_p95: percentile(
            &mut counters.stress_receive_drain_work_us,
            95,
        ),
        playout_pull_work_us_p95: percentile(&mut counters.playout_pull_work_us, 95),
        stress_playout_pull_work_us_p95: percentile(&mut counters.stress_playout_pull_work_us, 95),
        playout_lateness_us_max: counters.playout_lateness_us_max,
        neteq_concealed_samples: concealed_samples,
        neteq_concealed_percent: {
            let possible_samples = counters.receiver_pull_samples;
            if possible_samples > 0 {
                concealed_samples as f64 / possible_samples as f64 * 100.0
            } else {
                0.0
            }
        },
        neteq_receiver_errors: receiver_errors,
        neteq_max_current_buffer_ms: max_current_buffer_ms,
        neteq_max_target_delay_ms: max_target_delay_ms,
        receiver_creations,
        receiver_reuses,
        receiver_retirements,
        max_concurrent_receivers,
        max_receiver_pool,
        participants_metrics,
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

fn build_listener_receivers(
    listener: usize,
    config: &Config,
    encoder_count: usize,
) -> Result<Vec<Option<ReceiverSlot>>> {
    let mut receivers = Vec::with_capacity(encoder_count);
    for speaker in 0..encoder_count {
        if listener == speaker || config.scenario == Scenario::GameInterest {
            receivers.push(None);
        } else {
            receivers.push(Some(new_receiver_slot()?));
        }
    }
    Ok(receivers)
}

fn new_receiver_slot() -> Result<ReceiverSlot> {
    Ok(ReceiverSlot {
        receiver: VoiceReceiver::new(SAMPLE_RATE)?,
        pending_talkspurt_start_us: None,
        reported_concealed_samples: 0,
        reported_error: false,
    })
}

fn collect_receiver_totals(slot: &mut ReceiverSlot, totals: &mut ReceiverTotals) {
    let stats = slot.receiver.stats();
    totals.concealed_samples = totals.concealed_samples.saturating_add(
        stats
            .concealed_samples
            .saturating_sub(slot.reported_concealed_samples),
    );
    slot.reported_concealed_samples = stats.concealed_samples;
    if stats.sticky_error.is_some() && !slot.reported_error {
        totals.receiver_errors += 1;
        slot.reported_error = true;
    }
    totals.max_current_buffer_ms = totals
        .max_current_buffer_ms
        .max(stats.current_buffer_size_ms);
    totals.max_target_delay_ms = totals.max_target_delay_ms.max(stats.target_delay_ms);
}

#[allow(clippy::too_many_arguments)] // Explicit task inputs keep virtual-client state visible.
async fn run_listener(
    listener: usize,
    participants: usize,
    mut receive_rx: mpsc::UnboundedReceiver<ReceivedDatagram>,
    mut receivers: Vec<Option<ReceiverSlot>>,
    media_start: Instant,
    duration: Duration,
    clock_start: Instant,
    config: Config,
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
    let mut interest_active = vec![false; receivers.len()];
    let mut interest_started_us = vec![None; receivers.len()];
    let mut receiver_creations = match config.scenario {
        Scenario::Baseline => receivers.iter().flatten().count() as u64,
        Scenario::GameInterest => 0,
    };
    let mut receiver_retirements = 0_u64;
    let mut receiver_reuses = 0_u64;
    let mut receiver_pool = Vec::new();
    let mut receiver_totals = ReceiverTotals::default();
    let mut max_concurrent_receivers = receivers.iter().flatten().count();
    let mut max_receiver_pool = 0_usize;

    while Instant::now() < media_end {
        let scheduled = ticker.tick().await;
        let now = tokio::time::Instant::now();
        let media_frame = media_frame_at(media_start, Instant::now());
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
        let lateness_us = now.saturating_duration_since(scheduled).as_micros() as u64;
        counters.playout_lateness_us_max = counters.playout_lateness_us_max.max(lateness_us);
        let deadline_misses = skipped_ticks + u64::from(lateness_us > DEADLINE_LATE_US);
        counters.playout_deadline_misses += deadline_misses;
        if stress_active(&config, media_frame) {
            counters.stress_playout_ticks += skipped_ticks + 1;
            counters.stress_playout_deadline_misses += deadline_misses;
        }

        let callback_work_start = Instant::now();
        let stress = stress_active(&config, media_frame);
        sync_interest(
            listener,
            media_frame,
            &config,
            clock_start,
            &mut receivers,
            &mut interest_active,
            &mut interest_started_us,
            &mut receiver_retirements,
            &mut receiver_pool,
            &mut receiver_totals,
        )?;

        let drain_work_start = Instant::now();
        drain_received(
            listener,
            &mut receive_rx,
            &mut receivers,
            &mut interest_started_us,
            &config,
            media_frame,
            clock_start,
            &mut counters,
            &mut receiver_creations,
            &mut receiver_reuses,
            &mut receiver_pool,
        )?;
        let drain_work_us = drain_work_start.elapsed().as_micros() as u64;
        counters.receive_drain_work_us.push(drain_work_us);
        if stress {
            counters.stress_receive_drain_work_us.push(drain_work_us);
        }
        let pull_work_start = Instant::now();
        pull_receivers(
            listener,
            media_frame,
            &config,
            clock_start,
            &mut receivers,
            &mut counters,
        );
        let pull_work_us = pull_work_start.elapsed().as_micros() as u64;
        counters.playout_pull_work_us.push(pull_work_us);
        if stress {
            counters.stress_playout_pull_work_us.push(pull_work_us);
        }
        let callback_work_us = callback_work_start.elapsed().as_micros() as u64;
        counters.listener_callback_work_us.push(callback_work_us);
        if stress {
            counters
                .stress_listener_callback_work_us
                .push(callback_work_us);
        }
        max_concurrent_receivers = max_concurrent_receivers.max(receivers.iter().flatten().count());
        max_receiver_pool = max_receiver_pool.max(receiver_pool.len());
    }

    drain_received(
        listener,
        &mut receive_rx,
        &mut receivers,
        &mut interest_started_us,
        &config,
        media_frame_at(media_start, Instant::now()),
        clock_start,
        &mut counters,
        &mut receiver_creations,
        &mut receiver_reuses,
        &mut receiver_pool,
    )?;
    let mut result = ListenerResult {
        participant: listener,
        counters,
        active_receiver_count: receivers.iter().flatten().count(),
        receiver_creations,
        receiver_reuses,
        receiver_retirements,
        max_concurrent_receivers,
        max_receiver_pool,
        ..Default::default()
    };
    let delivery_deadline = Instant::now() + DELIVERY_GRACE;
    while Instant::now() < delivery_deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
        drain_received(
            listener,
            &mut receive_rx,
            &mut receivers,
            &mut interest_started_us,
            &config,
            media_frame_at(media_start, Instant::now()),
            clock_start,
            &mut result.counters,
            &mut result.receiver_creations,
            &mut result.receiver_reuses,
            &mut receiver_pool,
        )?;
    }
    result.active_receiver_count = receivers.iter().flatten().count();
    result.max_concurrent_receivers = result
        .max_concurrent_receivers
        .max(result.active_receiver_count);
    result.max_receiver_pool = result.max_receiver_pool.max(receiver_pool.len());
    for slot in receivers
        .iter_mut()
        .flatten()
        .chain(receiver_pool.iter_mut())
    {
        collect_receiver_totals(slot, &mut receiver_totals);
    }
    result.concealed_samples = receiver_totals.concealed_samples;
    result.receiver_errors = receiver_totals.receiver_errors;
    result.max_current_buffer_ms = receiver_totals.max_current_buffer_ms;
    result.max_target_delay_ms = receiver_totals.max_target_delay_ms;
    Ok(result)
}

#[allow(clippy::too_many_arguments)] // Keep each independent sender's clock and transport explicit.
async fn send_media(
    speaker: usize,
    sender_count: usize,
    duration: Duration,
    connections: Vec<Option<Connection>>,
    mut encoder: VoiceEncoder,
    media_start: Instant,
    clock_start: Instant,
    config: Config,
) -> Result<SendCounters> {
    let media_end = media_start + duration;
    // Independent clients do not share an encoder clock. Distribute their
    // phases across a frame instead of serializing every encoder in one task.
    let phase_nanos = SEND_TICK.as_nanos() * speaker as u128 / sender_count as u128;
    let first_tick = media_start + Duration::from_nanos(phase_nanos as u64);
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::from_std(first_tick), SEND_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut previous_scheduled = None;
    let mut frame_index = 0_u64;
    let mut counters = SendCounters::default();
    let mut previous_stress = false;

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
        let stress = stress_active(&config, frame_index);
        if stress && !previous_stress {
            counters.stress_events += 1;
        }
        if stress {
            counters.stress_sender_ticks += skipped_ticks + 1;
            counters.stress_sender_skipped_ticks += skipped_ticks;
        }
        let callback_work_start = Instant::now();
        encode_and_send(
            speaker,
            &connections,
            &mut encoder,
            frame_index,
            clock_start,
            &config,
            &mut counters,
        )?;
        let callback_work_us = callback_work_start.elapsed().as_micros() as u64;
        counters.callback_work_us.push(callback_work_us);
        if stress {
            counters.stress_callback_work_us.push(callback_work_us);
        }
        previous_stress = stress;
        frame_index += 1;
    }
    Ok(counters)
}

fn encode_and_send(
    speaker: usize,
    connections: &[Option<Connection>],
    encoder: &mut VoiceEncoder,
    frame_index: u64,
    clock_start: Instant,
    config: &Config,
    counters: &mut SendCounters,
) -> Result<()> {
    let stress = stress_active(config, frame_index);
    let active = speaker_active(config, frame_index, speaker);
    let pcm = speech_frame(frame_index, speaker, active);
    encoder.push_pcm(&pcm);
    let Some(packet) = encoder.poll_packet()? else {
        return Ok(());
    };
    counters.encoded_packets += 1;

    let sent_at_us = clock_start.elapsed().as_micros() as u64;
    let mut wire = Vec::with_capacity(ENVELOPE_HEADER_LEN + VoicePacket::HEADER_LEN + 512);
    wire.extend_from_slice(&sent_at_us.to_be_bytes());
    wire.extend_from_slice(&frame_index.to_be_bytes());
    packet.encode_to_bytes(&mut wire);
    let wire = Bytes::from(wire);
    let fanout_start = Instant::now();
    for (listener, connection) in connections.iter().enumerate().take(config.participants) {
        if listener == speaker {
            continue;
        }
        if config.scenario == Scenario::GameInterest
            && config.delivery == Delivery::SenderFiltered
            && !listener_interested(config, frame_index, speaker, listener)
        {
            continue;
        }
        let connection = connection
            .as_ref()
            .context("missing full-mesh connection")?;
        match connection.send_datagram(wire.clone()) {
            Ok(()) => {
                counters.sent_datagrams += 1;
                counters.sent_bytes += wire.len() as u64;
                if stress {
                    counters.stress_sent_datagrams += 1;
                }
            }
            Err(_) => counters.send_errors += 1,
        }
    }
    let fanout_span_us = fanout_start.elapsed().as_micros() as u64;
    counters.fanout_spans_us.push(fanout_span_us);
    if stress {
        counters.stress_fanout_spans_us.push(fanout_span_us);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Hot-path state is borrowed separately to avoid hidden sharing.
fn drain_received(
    listener: usize,
    receive_rx: &mut mpsc::UnboundedReceiver<ReceivedDatagram>,
    receivers: &mut [Option<ReceiverSlot>],
    interest_started_us: &mut [Option<u64>],
    config: &Config,
    current_frame_index: u64,
    clock_start: Instant,
    counters: &mut RunCounters,
    receiver_creations: &mut u64,
    receiver_reuses: &mut u64,
    receiver_pool: &mut Vec<ReceiverSlot>,
) -> Result<()> {
    while let Ok(datagram) = receive_rx.try_recv() {
        counters.received_datagrams += 1;
        counters.received_bytes += datagram.bytes.len() as u64;
        let processed_at_us = clock_start.elapsed().as_micros() as u64;
        let queue_delay_us = processed_at_us.saturating_sub(datagram.received_at_us);
        counters.queue_delays_us.push(queue_delay_us);

        if datagram.bytes.len() < ENVELOPE_HEADER_LEN {
            counters.malformed_datagrams += 1;
            continue;
        }
        let sent_at_us = u64::from_be_bytes(
            datagram.bytes[..8]
                .try_into()
                .expect("checked envelope length"),
        );
        let frame_index = u64::from_be_bytes(
            datagram.bytes[8..ENVELOPE_HEADER_LEN]
                .try_into()
                .expect("checked envelope length"),
        );
        if stress_active(config, frame_index) {
            counters.stress_queue_delays_us.push(queue_delay_us);
        }
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
        if !listener_interested(config, frame_index, datagram.speaker, listener)
            || !listener_interested(config, current_frame_index, datagram.speaker, listener)
        {
            counters.outside_interest_datagrams += 1;
            continue;
        }
        counters.accepted_datagrams += 1;

        let slot = receivers
            .get_mut(datagram.speaker)
            .context("received speaker outside receiver table")?;
        if slot.is_none() {
            if config.receiver_policy == ReceiverPolicy::Pool {
                if let Some(reused) = receiver_pool.pop() {
                    *slot = Some(reused);
                    *receiver_reuses += 1;
                }
            }
            if slot.is_none() {
                *slot = Some(new_receiver_slot()?);
                *receiver_creations += 1;
            }
        }
        let slot = slot.as_mut().expect("receiver was just initialized");
        if !packet.payload.is_empty() {
            if let Some(interest_start_us) = interest_started_us
                .get_mut(datagram.speaker)
                .and_then(Option::take)
            {
                counters
                    .interest_entry_to_first_media_us
                    .push(datagram.received_at_us.saturating_sub(interest_start_us));
            }
        }
        if packet.flags.contains(PacketFlags::START_OF_TALKSPURT) {
            slot.pending_talkspurt_start_us = Some(sent_at_us);
        }
        slot.receiver.push_packet_with_now_mono(
            packet,
            PacketArrival {
                received_at_mono_us: datagram.received_at_us,
            },
            processed_at_us,
        )?;
    }
    Ok(())
}

fn pull_receivers(
    listener: usize,
    frame_index: u64,
    config: &Config,
    clock_start: Instant,
    receivers: &mut [Option<ReceiverSlot>],
    counters: &mut RunCounters,
) {
    let mut frame = [0.0_f32; PULL_FRAME_SAMPLES];
    for (speaker, slot) in receivers.iter_mut().enumerate() {
        if !listener_interested(config, frame_index, speaker, listener) {
            continue;
        }
        let Some(slot) = slot else {
            continue;
        };
        slot.receiver.pull_frame(&mut frame);
        counters.receiver_pull_samples += frame.len() as u64;
        if slot.pending_talkspurt_start_us.is_some() && rms(&frame) > NON_SILENT_RMS {
            let started_us = slot
                .pending_talkspurt_start_us
                .take()
                .expect("checked pending start");
            counters
                .talkspurt_start_to_audio_us
                .push((clock_start.elapsed().as_micros() as u64).saturating_sub(started_us));
        }
    }
}

#[allow(clippy::too_many_arguments)] // Interest transitions update several independent measurements.
fn sync_interest(
    listener: usize,
    frame_index: u64,
    config: &Config,
    clock_start: Instant,
    receivers: &mut [Option<ReceiverSlot>],
    interest_active: &mut [bool],
    interest_started_us: &mut [Option<u64>],
    receiver_retirements: &mut u64,
    receiver_pool: &mut Vec<ReceiverSlot>,
    receiver_totals: &mut ReceiverTotals,
) -> Result<()> {
    for speaker in 0..receivers.len() {
        let interested = listener_interested(config, frame_index, speaker, listener);
        if interested && !interest_active[speaker] {
            interest_started_us[speaker] = Some(clock_start.elapsed().as_micros() as u64);
        } else if !interested && interest_active[speaker] {
            interest_started_us[speaker] = None;
            if let Some(mut retired) = receivers[speaker].take() {
                *receiver_retirements += 1;
                collect_receiver_totals(&mut retired, receiver_totals);
                if config.receiver_policy == ReceiverPolicy::Pool {
                    retired.receiver.reset_stream()?;
                    retired.pending_talkspurt_start_us = None;
                    receiver_pool.push(retired);
                }
            }
        }
        interest_active[speaker] = interested;
    }
    Ok(())
}

fn media_frame_at(media_start: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(media_start)
        .as_micros()
        .saturating_div(SEND_TICK.as_micros()) as u64
}

fn speaker_active(config: &Config, frame_index: u64, speaker: usize) -> bool {
    match config.scenario {
        Scenario::Baseline => talker_active(frame_index, speaker),
        Scenario::GameInterest => game_talker_active(config, frame_index, speaker),
    }
}

fn game_talker_active(config: &Config, frame_index: u64, speaker: usize) -> bool {
    if config.interest_profile == InterestProfile::CrowdBurst && stress_active(config, frame_index)
    {
        return true;
    }
    let turn = frame_index / GAME_TURN_FRAMES;
    let phase = frame_index % GAME_TURN_FRAMES;
    let transition = (turn + config.seed) % 3;
    for slot in 0..config.talkers {
        let current =
            (config.seed as usize + slot + turn as usize * config.talkers) % config.participants;
        let current_active = match transition {
            0 => phase < GAME_TALK_FRAMES,
            1 | 2 => true,
            _ => unreachable!(),
        };
        if current == speaker && current_active {
            return true;
        }
        if transition == 1 && phase >= GAME_TURN_FRAMES - GAME_OVERLAP_FRAMES {
            let next = (config.seed as usize + slot + (turn as usize + 1) * config.talkers)
                % config.participants;
            if next == speaker {
                return true;
            }
        }
    }
    false
}

fn listener_interested(config: &Config, frame_index: u64, speaker: usize, listener: usize) -> bool {
    if speaker == listener {
        return false;
    }
    if config.scenario == Scenario::Baseline {
        return speaker < config.talkers;
    }

    if config.interest_profile == InterestProfile::GroupMerge {
        let group_size = config.interest_listeners + 1;
        let speaker_group = speaker / group_size;
        let listener_group = listener / group_size;
        return if stress_active(config, frame_index) {
            speaker_group / 2 == listener_group / 2
        } else {
            speaker_group == listener_group
        };
    }

    let available = config.participants - 1;
    let rotation = match config.interest_profile {
        InterestProfile::BoundaryOscillation => {
            let base = mix64(config.seed ^ (speaker as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
                as usize
                % available;
            if stress_active(config, frame_index) {
                let phase = frame_index % STRESS_CYCLE_FRAMES;
                let alternate =
                    ((phase - STRESS_WINDOW_START_FRAMES) / BOUNDARY_OSCILLATION_FRAMES) % 2;
                (base + alternate as usize * config.interest_listeners) % available
            } else {
                base
            }
        }
        InterestProfile::Rotating | InterestProfile::CrowdBurst => {
            let epoch = frame_index / GAME_INTEREST_EPOCH_FRAMES;
            mix64(
                config.seed
                    ^ (speaker as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    ^ epoch.wrapping_mul(0xbf58_476d_1ce4_e5b9),
            ) as usize
                % available
        }
        InterestProfile::GroupMerge => unreachable!("handled above"),
    };
    (0..config.interest_listeners).any(|rank| {
        let offset = 1 + (rotation + rank) % available;
        (speaker + offset) % config.participants == listener
    })
}

fn stress_active(config: &Config, frame_index: u64) -> bool {
    if config.scenario != Scenario::GameInterest {
        return false;
    }
    let phase = frame_index % STRESS_CYCLE_FRAMES;
    match config.interest_profile {
        InterestProfile::Rotating => false,
        InterestProfile::CrowdBurst => {
            (CROWD_BURST_START_FRAMES..CROWD_BURST_END_FRAMES).contains(&phase)
        }
        InterestProfile::GroupMerge | InterestProfile::BoundaryOscillation => {
            (STRESS_WINDOW_START_FRAMES..STRESS_WINDOW_END_FRAMES).contains(&phase)
        }
    }
}

fn max_interest_listeners(config: &Config) -> usize {
    let sample_frames = [0, STRESS_WINDOW_START_FRAMES, CROWD_BURST_START_FRAMES];
    sample_frames
        .into_iter()
        .flat_map(|frame| {
            (0..config.participants).map(move |speaker| {
                (0..config.participants)
                    .filter(|listener| listener_interested(config, frame, speaker, *listener))
                    .count()
            })
        })
        .max()
        .unwrap_or(0)
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

fn participant_metrics(listener: &mut ListenerResult) -> ParticipantMetrics {
    ParticipantMetrics {
        participant: listener.participant,
        received_datagrams: listener.counters.received_datagrams,
        accepted_datagrams: listener.counters.accepted_datagrams,
        outside_interest_datagrams: listener.counters.outside_interest_datagrams,
        active_receiver_count: listener.active_receiver_count,
        receiver_creations: listener.receiver_creations,
        receiver_reuses: listener.receiver_reuses,
        receiver_retirements: listener.receiver_retirements,
        max_concurrent_receivers: listener.max_concurrent_receivers,
        max_receiver_pool: listener.max_receiver_pool,
        receive_queue_delay_us_p95: percentile(&mut listener.counters.queue_delays_us, 95),
        playout_skipped_ticks: listener.counters.playout_skipped_ticks,
        playout_deadline_miss_percent: if listener.counters.playout_ticks > 0 {
            listener.counters.playout_deadline_misses as f64
                / listener.counters.playout_ticks as f64
                * 100.0
        } else {
            0.0
        },
        stress_playout_deadline_miss_percent: percent(
            listener.counters.stress_playout_deadline_misses,
            listener.counters.stress_playout_ticks,
        ),
        playout_lateness_us_max: listener.counters.playout_lateness_us_max,
        interest_entry_to_first_media_us_p95: percentile(
            &mut listener.counters.interest_entry_to_first_media_us,
            95,
        ),
        interest_entry_events: listener.counters.interest_entry_to_first_media_us.len(),
        talkspurt_start_to_audio_us_p95: percentile(
            &mut listener.counters.talkspurt_start_to_audio_us,
            95,
        ),
        talkspurt_audio_events: listener.counters.talkspurt_start_to_audio_us.len(),
        neteq_concealed_samples: listener.concealed_samples,
        neteq_receiver_errors: listener.receiver_errors,
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

fn percent(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64 * 100.0
    }
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

fn current_rss_kib() -> Result<i64> {
    let status = fs::read_to_string("/proc/self/status").context("read /proc/self/status")?;
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .context("VmRSS missing from /proc/self/status")?;
    let kib = line
        .split_whitespace()
        .nth(1)
        .context("VmRSS value missing")?
        .parse::<i64>()
        .context("parse VmRSS")?;
    Ok(kib)
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

    #[test]
    fn game_schedule_rotates_through_every_participant() {
        let config = Config {
            participants: 32,
            talkers: 4,
            scenario: Scenario::GameInterest,
            ..Config::default()
        };
        let frames = GAME_TURN_FRAMES * (config.participants / config.talkers) as u64;
        for speaker in 0..config.participants {
            assert!((0..frames).any(|frame| game_talker_active(&config, frame, speaker)));
        }
        for frame in 0..frames {
            let active = (0..config.participants)
                .filter(|speaker| game_talker_active(&config, frame, *speaker))
                .count();
            assert!(active <= config.talkers * 2);
        }
    }

    #[test]
    fn game_interest_has_exact_fanout_and_changes() {
        let config = Config {
            participants: 32,
            talkers: 4,
            scenario: Scenario::GameInterest,
            interest_listeners: 7,
            seed: 9,
            ..Config::default()
        };
        for speaker in 0..config.participants {
            let first = (0..config.participants)
                .filter(|listener| listener_interested(&config, 0, speaker, *listener))
                .collect::<Vec<_>>();
            let second = (0..config.participants)
                .filter(|listener| {
                    listener_interested(&config, GAME_INTEREST_EPOCH_FRAMES, speaker, *listener)
                })
                .collect::<Vec<_>>();
            assert_eq!(first.len(), config.interest_listeners);
            assert_eq!(second.len(), config.interest_listeners);
            assert!(!first.contains(&speaker));
            assert_ne!(first, second);
        }
    }

    #[test]
    fn crowd_burst_activates_every_participant_for_one_second() {
        let config = Config {
            participants: 32,
            talkers: 4,
            scenario: Scenario::GameInterest,
            interest_profile: InterestProfile::CrowdBurst,
            ..Config::default()
        };
        let before = (0..config.participants)
            .filter(|speaker| game_talker_active(&config, CROWD_BURST_START_FRAMES - 1, *speaker))
            .count();
        let during = (0..config.participants)
            .filter(|speaker| game_talker_active(&config, CROWD_BURST_START_FRAMES, *speaker))
            .count();
        assert!(before <= config.talkers * 2);
        assert_eq!(during, config.participants);
        assert!(stress_active(&config, CROWD_BURST_END_FRAMES - 1));
        assert!(!stress_active(&config, CROWD_BURST_END_FRAMES));
    }

    #[test]
    fn group_merge_doubles_complete_group_interest() {
        let config = Config {
            participants: 32,
            talkers: 4,
            scenario: Scenario::GameInterest,
            interest_profile: InterestProfile::GroupMerge,
            interest_listeners: 7,
            ..Config::default()
        };
        for speaker in 0..config.participants {
            let split = (0..config.participants)
                .filter(|listener| listener_interested(&config, 0, speaker, *listener))
                .count();
            let merged = (0..config.participants)
                .filter(|listener| {
                    listener_interested(&config, STRESS_WINDOW_START_FRAMES, speaker, *listener)
                })
                .count();
            assert_eq!(split, 7);
            assert_eq!(merged, 15);
        }
        assert_eq!(max_interest_listeners(&config), 15);
    }

    #[test]
    fn boundary_oscillation_uses_disjoint_listener_sets() {
        let config = Config {
            participants: 32,
            talkers: 4,
            scenario: Scenario::GameInterest,
            interest_profile: InterestProfile::BoundaryOscillation,
            interest_listeners: 7,
            seed: 9,
            ..Config::default()
        };
        for speaker in 0..config.participants {
            let first = (0..config.participants)
                .filter(|listener| {
                    listener_interested(&config, STRESS_WINDOW_START_FRAMES, speaker, *listener)
                })
                .collect::<Vec<_>>();
            let second = (0..config.participants)
                .filter(|listener| {
                    listener_interested(
                        &config,
                        STRESS_WINDOW_START_FRAMES + BOUNDARY_OSCILLATION_FRAMES,
                        speaker,
                        *listener,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(first.len(), config.interest_listeners);
            assert_eq!(second.len(), config.interest_listeners);
            assert!(first.iter().all(|listener| !second.contains(listener)));
        }
    }
}

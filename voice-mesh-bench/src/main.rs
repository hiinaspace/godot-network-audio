use std::{
    collections::BinaryHeap,
    env, fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};

const METRIC_SAMPLE_CAPACITY: usize = 4_096;
const NETEQ_TIMELINE_CAPACITY_PER_PARTICIPANT: usize = 3_600;

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
const ENVELOPE_HEADER_LEN: usize = 24;
const PLAYOUT_TICK: Duration = Duration::from_millis(10);
const SEND_TICK: Duration = Duration::from_millis(20);
const STARTUP_SETTLE: Duration = Duration::from_millis(100);
const DELIVERY_GRACE: Duration = Duration::from_millis(300);
const DEADLINE_LATE_US: u64 = 2_000;
const TARGET_DELAY_NOTICEABLE_MS: u32 = 100;
const TARGET_DELAY_HIGH_MS: u32 = 150;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topology {
    Direct,
    Star,
}

impl Topology {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct-full-mesh",
            Self::Star => "authoritative-star",
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaImpairment {
    None,
    UniformLoss,
    BurstLoss,
    Outage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChurnProfile {
    None,
    Join,
    Leave,
    Reconnect,
    Replace,
}

impl ChurnProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Join => "join",
            Self::Leave => "leave",
            Self::Reconnect => "reconnect",
            Self::Replace => "replace",
        }
    }

    fn active(self) -> bool {
        self != Self::None
    }

    fn reconnects(self) -> bool {
        matches!(self, Self::Join | Self::Reconnect | Self::Replace)
    }
}

impl MediaImpairment {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UniformLoss => "uniform-loss",
            Self::BurstLoss => "burst-loss",
            Self::Outage => "outage",
        }
    }
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
    topology: Topology,
    participants: usize,
    talkers: usize,
    duration: Duration,
    dtx: bool,
    scenario: Scenario,
    delivery: Delivery,
    receiver_policy: ReceiverPolicy,
    interest_profile: InterestProfile,
    interest_listeners: usize,
    media_impairment: MediaImpairment,
    media_loss_percent: f64,
    media_burst_ms: u64,
    media_outage_start_ms: u64,
    media_outage_duration_ms: u64,
    churn_profile: ChurnProfile,
    churn_participant: usize,
    churn_start_ms: u64,
    churn_downtime_ms: u64,
    seed: u64,
    runtime_workers: usize,
    output: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            topology: Topology::Direct,
            participants: 4,
            talkers: 1,
            duration: Duration::from_secs(5),
            dtx: true,
            scenario: Scenario::Baseline,
            delivery: Delivery::SenderFiltered,
            receiver_policy: ReceiverPolicy::Retire,
            interest_profile: InterestProfile::Rotating,
            interest_listeners: 7,
            media_impairment: MediaImpairment::None,
            media_loss_percent: 3.0,
            media_burst_ms: 60,
            media_outage_start_ms: 3_000,
            media_outage_duration_ms: 300,
            churn_profile: ChurnProfile::None,
            churn_participant: 0,
            churn_start_ms: 6_000,
            churn_downtime_ms: 1_000,
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
                "--topology" => {
                    let value = args.next().context("--topology requires a value")?;
                    config.topology = match value.as_str() {
                        "direct" => Topology::Direct,
                        "star" => Topology::Star,
                        _ => bail!("--topology must be direct or star, got {value}"),
                    };
                }
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
                "--media-impairment" => {
                    let value = args.next().context("--media-impairment requires a value")?;
                    config.media_impairment = match value.as_str() {
                        "none" => MediaImpairment::None,
                        "uniform-loss" => MediaImpairment::UniformLoss,
                        "burst-loss" => MediaImpairment::BurstLoss,
                        "outage" => MediaImpairment::Outage,
                        _ => bail!(
                            "--media-impairment must be none, uniform-loss, burst-loss, or outage, got {value}"
                        ),
                    };
                }
                "--media-loss-percent" => {
                    config.media_loss_percent = parse_next(&mut args, "--media-loss-percent")?;
                }
                "--media-burst-ms" => {
                    config.media_burst_ms = parse_next(&mut args, "--media-burst-ms")?;
                }
                "--media-outage-start-ms" => {
                    config.media_outage_start_ms =
                        parse_next(&mut args, "--media-outage-start-ms")?;
                }
                "--media-outage-duration-ms" => {
                    config.media_outage_duration_ms =
                        parse_next(&mut args, "--media-outage-duration-ms")?;
                }
                "--churn" => {
                    let value = args.next().context("--churn requires a value")?;
                    config.churn_profile = match value.as_str() {
                        "none" => ChurnProfile::None,
                        "join" => ChurnProfile::Join,
                        "leave" => ChurnProfile::Leave,
                        "reconnect" => ChurnProfile::Reconnect,
                        "replace" => ChurnProfile::Replace,
                        _ => bail!(
                            "--churn must be none, join, leave, reconnect, or replace, got {value}"
                        ),
                    };
                }
                "--churn-participant" => {
                    config.churn_participant = parse_next(&mut args, "--churn-participant")?;
                }
                "--churn-start-ms" => {
                    config.churn_start_ms = parse_next(&mut args, "--churn-start-ms")?;
                }
                "--churn-downtime-ms" => {
                    config.churn_downtime_ms = parse_next(&mut args, "--churn-downtime-ms")?;
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
        if !config.media_loss_percent.is_finite()
            || !(0.0..=100.0).contains(&config.media_loss_percent)
        {
            bail!("--media-loss-percent must be finite and between 0 and 100");
        }
        if config.media_impairment == MediaImpairment::BurstLoss
            && (config.media_loss_percent <= 0.0
                || config.media_loss_percent >= 100.0
                || config.media_burst_ms < SEND_TICK.as_millis() as u64)
        {
            bail!("burst loss requires 0 < --media-loss-percent < 100 and --media-burst-ms >= 20");
        }
        if config.media_impairment == MediaImpairment::Outage
            && config.media_outage_duration_ms == 0
        {
            bail!("outage requires --media-outage-duration-ms greater than zero");
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
        if config.churn_profile.active() {
            if config.topology != Topology::Direct {
                bail!("churn profiles currently require --topology direct");
            }
            if config.churn_participant >= config.participants {
                bail!("--churn-participant must be less than participants");
            }
            if matches!(
                config.churn_profile,
                ChurnProfile::Reconnect | ChurnProfile::Replace
            ) && config.churn_downtime_ms == 0
            {
                bail!("--churn-downtime-ms must be greater than zero");
            }
            let event_end_ms = if matches!(
                config.churn_profile,
                ChurnProfile::Reconnect | ChurnProfile::Replace
            ) {
                config
                    .churn_start_ms
                    .saturating_add(config.churn_downtime_ms)
            } else {
                config.churn_start_ms
            };
            if event_end_ms >= config.duration.as_millis() as u64 {
                bail!("churn start plus downtime must be shorter than the media duration");
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

#[derive(Debug)]
struct ImpairmentRouteState {
    rng_state: u64,
    burst_bad: bool,
}

impl ImpairmentRouteState {
    fn new(seed: u64, speaker: usize, listener: usize) -> Self {
        Self {
            rng_state: mix64(
                seed ^ (speaker as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    ^ (listener as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9),
            ),
            burst_bad: false,
        }
    }

    fn should_drop(&mut self, config: &Config, frame_index: u64) -> bool {
        match config.media_impairment {
            MediaImpairment::None => false,
            MediaImpairment::UniformLoss => self.random_unit() < config.media_loss_percent / 100.0,
            MediaImpairment::BurstLoss => {
                let mean_burst_packets =
                    (config.media_burst_ms as f64 / SEND_TICK.as_millis() as f64).max(1.0);
                let exit_probability = 1.0 / mean_burst_packets;
                let loss_fraction = config.media_loss_percent / 100.0;
                let enter_probability =
                    (loss_fraction * exit_probability / (1.0 - loss_fraction)).min(1.0);
                if self.burst_bad {
                    if self.random_unit() < exit_probability {
                        self.burst_bad = false;
                        false
                    } else {
                        true
                    }
                } else if self.random_unit() < enter_probability {
                    self.burst_bad = true;
                    true
                } else {
                    false
                }
            }
            MediaImpairment::Outage => {
                let packet_time_ms = frame_index.saturating_mul(SEND_TICK.as_millis() as u64);
                let outage_end = config
                    .media_outage_start_ms
                    .saturating_add(config.media_outage_duration_ms);
                (config.media_outage_start_ms..outage_end).contains(&packet_time_ms)
            }
        }
    }

    fn random_unit(&mut self) -> f64 {
        self.rng_state = self.rng_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix64(self.rng_state) as f64 / u64::MAX as f64
    }
}

type ConnectionSlot = Arc<RwLock<Option<Connection>>>;
type EndpointSlot = Arc<RwLock<Option<Endpoint>>>;

struct Mesh {
    endpoints: Vec<EndpointSlot>,
    sfu_endpoint: Option<Endpoint>,
    connections: Vec<Vec<ConnectionSlot>>,
    receive_txs: Vec<mpsc::UnboundedSender<ReceivedDatagram>>,
    readers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    sfu_counters: Arc<SfuCounters>,
}

impl Mesh {
    async fn close(self) {
        for endpoint in &self.endpoints {
            let endpoint = { endpoint.read().expect("endpoint lock poisoned").clone() };
            if let Some(endpoint) = endpoint {
                endpoint.close().await;
            }
        }
        if let Some(endpoint) = self.sfu_endpoint {
            endpoint.close().await;
        }
        for reader in self.readers.lock().expect("reader lock poisoned").drain(..) {
            reader.abort();
        }
    }
}

#[derive(Debug, Default)]
struct SfuCounters {
    received_datagrams: AtomicU64,
    received_bytes: AtomicU64,
    forwarded_datagrams: AtomicU64,
    forwarded_bytes: AtomicU64,
    send_errors: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy)]
struct SfuSnapshot {
    received_datagrams: u64,
    received_bytes: u64,
    forwarded_datagrams: u64,
    forwarded_bytes: u64,
    send_errors: u64,
}

impl SfuCounters {
    fn snapshot(&self) -> SfuSnapshot {
        SfuSnapshot {
            received_datagrams: self.received_datagrams.load(Ordering::Relaxed),
            received_bytes: self.received_bytes.load(Ordering::Relaxed),
            forwarded_datagrams: self.forwarded_datagrams.load(Ordering::Relaxed),
            forwarded_bytes: self.forwarded_bytes.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default)]
struct ChurnResult {
    disconnects: u64,
    reconnects: u64,
    reconnect_errors: u64,
    reconnect_duration_ms: f64,
    new_identity: bool,
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
    allocator_arena_kib: u64,
    allocator_in_use_kib: u64,
    allocator_free_kib: u64,
    allocator_mmap_kib: u64,
}

#[derive(Debug, Serialize)]
struct NetEqTimelineSample {
    participant: usize,
    elapsed_seconds: f64,
    active_receivers: usize,
    current_buffer_ms_max: u32,
    target_delay_ms_max: u32,
    concealed_samples: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct AllocatorUsage {
    arena_kib: u64,
    in_use_kib: u64,
    free_kib: u64,
    mmap_kib: u64,
}

#[derive(Debug, Serialize)]
struct Metrics {
    schema_version: u32,
    metric_sample_capacity: usize,
    topology: &'static str,
    scenario: &'static str,
    delivery: &'static str,
    receiver_policy: &'static str,
    interest_profile: &'static str,
    media_impairment: &'static str,
    media_loss_percent: f64,
    media_burst_ms: u64,
    media_outage_start_ms: u64,
    media_outage_duration_ms: u64,
    churn_profile: &'static str,
    churn_participant: usize,
    churn_start_ms: u64,
    churn_downtime_ms: u64,
    churn_disconnects: u64,
    churn_reconnects: u64,
    churn_reconnect_errors: u64,
    churn_reconnect_duration_ms: f64,
    churn_new_identity: bool,
    affected_route_max_transport_gap_ms: f64,
    unaffected_route_max_transport_gap_ms: f64,
    seed: u64,
    runtime_worker_threads: usize,
    opus_version: &'static str,
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
    neteq_timeline: Vec<NetEqTimelineSample>,
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
    media_impairment_attempted_datagrams: u64,
    media_impairment_delivered_datagrams: u64,
    media_impairment_dropped_datagrams: u64,
    malformed_datagrams: u64,
    missing_datagrams: u64,
    sent_bytes: u64,
    received_bytes: u64,
    outbound_mbit_per_second: f64,
    sfu_received_datagrams: u64,
    sfu_received_bytes: u64,
    sfu_forwarded_datagrams: u64,
    sfu_forwarded_bytes: u64,
    sfu_send_errors: u64,
    sfu_outbound_mbit_per_second: f64,
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
    neteq_concealment_events: u64,
    neteq_silent_concealed_samples: u64,
    neteq_late_packets_discarded: u64,
    neteq_inserted_samples_for_deceleration: u64,
    neteq_removed_samples_for_acceleration: u64,
    neteq_receiver_errors: usize,
    neteq_max_current_buffer_ms: u32,
    neteq_max_target_delay_ms: u32,
    neteq_target_delay_observations: u64,
    neteq_target_delay_ge_100_ms_percent: f64,
    neteq_target_delay_ge_150_ms_percent: f64,
    neteq_target_delay_ge_100_ms_max_continuous_ms: u64,
    neteq_target_delay_ge_150_ms_max_continuous_ms: u64,
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
    media_impairment_attempted_datagrams: u64,
    media_impairment_delivered_datagrams: u64,
    media_impairment_dropped_datagrams: u64,
    affected_route_max_transport_gap_ms: f64,
    unaffected_route_max_transport_gap_ms: f64,
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
    neteq_concealment_events: u64,
    neteq_silent_concealed_samples: u64,
    neteq_late_packets_discarded: u64,
    neteq_receiver_errors: usize,
    neteq_target_delay_observations: u64,
    neteq_target_delay_ge_100_ms_percent: f64,
    neteq_target_delay_ge_150_ms_percent: f64,
    neteq_target_delay_ge_100_ms_max_continuous_ms: u64,
    neteq_target_delay_ge_150_ms_max_continuous_ms: u64,
}

#[derive(Debug, Default)]
struct RunCounters {
    received_datagrams: u64,
    accepted_datagrams: u64,
    outside_interest_datagrams: u64,
    malformed_datagrams: u64,
    received_bytes: u64,
    media_impairment_attempted_datagrams: u64,
    media_impairment_delivered_datagrams: u64,
    media_impairment_dropped_datagrams: u64,
    latencies_us: MetricSamples,
    queue_delays_us: MetricSamples,
    stress_queue_delays_us: MetricSamples,
    listener_callback_work_us: MetricSamples,
    stress_listener_callback_work_us: MetricSamples,
    receive_drain_work_us: MetricSamples,
    stress_receive_drain_work_us: MetricSamples,
    playout_pull_work_us: MetricSamples,
    stress_playout_pull_work_us: MetricSamples,
    interest_entry_to_first_media_us: MetricSamples,
    talkspurt_start_to_audio_us: MetricSamples,
    receiver_pull_samples: u64,
    playout_ticks: u64,
    playout_callbacks: u64,
    playout_skipped_ticks: u64,
    playout_deadline_misses: u64,
    stress_playout_ticks: u64,
    stress_playout_deadline_misses: u64,
    playout_lateness_us_max: u64,
    affected_route_max_transport_gap_us: u64,
    unaffected_route_max_transport_gap_us: u64,
    target_delay_observations: u64,
    target_delay_ge_100_ms_observations: u64,
    target_delay_ge_150_ms_observations: u64,
    target_delay_ge_100_ms_max_continuous_ticks: u64,
    target_delay_ge_150_ms_max_continuous_ticks: u64,
    neteq_max_current_buffer_ms: u32,
    neteq_max_target_delay_ms: u32,
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
    fanout_spans_us: MetricSamples,
    stress_fanout_spans_us: MetricSamples,
    callback_work_us: MetricSamples,
    stress_callback_work_us: MetricSamples,
}

#[derive(Debug, Default)]
struct MetricSamples {
    samples: BinaryHeap<(u64, u64)>,
    seen: u64,
    max: u64,
    salt: u64,
}

impl MetricSamples {
    fn with_salt(salt: u64) -> Self {
        Self {
            salt,
            ..Self::default()
        }
    }

    fn push(&mut self, value: u64) {
        self.seen += 1;
        self.max = self.max.max(value);
        let priority = mix64(self.salt ^ self.seen.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        self.insert_candidate((priority, value));
    }

    fn merge(&mut self, other: Self) {
        self.seen += other.seen;
        self.max = self.max.max(other.max);
        for sample in other.samples {
            self.insert_candidate(sample);
        }
    }

    fn insert_candidate(&mut self, sample: (u64, u64)) {
        if self.samples.len() < METRIC_SAMPLE_CAPACITY {
            self.samples.push(sample);
        } else if self.samples.peek().is_some_and(|largest| sample < *largest) {
            self.samples.pop();
            self.samples.push(sample);
        }
    }

    fn percentile(&self, rank: usize) -> u64 {
        let mut values = self
            .samples
            .iter()
            .map(|(_, value)| *value)
            .collect::<Vec<_>>();
        percentile(&mut values, rank)
    }

    fn count(&self) -> u64 {
        self.seen
    }

    fn max(&self) -> u64 {
        self.max
    }
}

#[derive(Debug, Default)]
struct ListenerResult {
    participant: usize,
    counters: RunCounters,
    concealed_samples: u64,
    concealment_events: u64,
    silent_concealed_samples: u64,
    late_packets_discarded: u64,
    inserted_samples_for_deceleration: u64,
    removed_samples_for_acceleration: u64,
    receiver_errors: usize,
    max_current_buffer_ms: u32,
    max_target_delay_ms: u32,
    active_receiver_count: usize,
    receiver_creations: u64,
    receiver_reuses: u64,
    receiver_retirements: u64,
    max_concurrent_receivers: usize,
    max_receiver_pool: usize,
    neteq_timeline: Vec<NetEqTimelineSample>,
}

struct ReceiverSlot {
    receiver: VoiceReceiver,
    pending_talkspurt_start_us: Option<u64>,
    reported_concealed_samples: u64,
    reported_concealment_events: u64,
    reported_silent_concealed_samples: u64,
    reported_late_packets_discarded: u64,
    reported_inserted_samples_for_deceleration: u64,
    reported_removed_samples_for_acceleration: u64,
    reported_error: bool,
    target_delay_ge_100_ms_continuous_ticks: u64,
    target_delay_ge_150_ms_continuous_ticks: u64,
}

#[derive(Debug, Default)]
struct ReceiverTotals {
    concealed_samples: u64,
    concealment_events: u64,
    silent_concealed_samples: u64,
    late_packets_discarded: u64,
    inserted_samples_for_deceleration: u64,
    removed_samples_for_acceleration: u64,
    receiver_errors: usize,
    max_current_buffer_ms: u32,
    max_target_delay_ms: u32,
}

impl RunCounters {
    fn with_salt(salt: u64) -> Self {
        Self {
            latencies_us: MetricSamples::with_salt(salt ^ 1),
            queue_delays_us: MetricSamples::with_salt(salt ^ 2),
            stress_queue_delays_us: MetricSamples::with_salt(salt ^ 3),
            listener_callback_work_us: MetricSamples::with_salt(salt ^ 4),
            stress_listener_callback_work_us: MetricSamples::with_salt(salt ^ 5),
            receive_drain_work_us: MetricSamples::with_salt(salt ^ 6),
            stress_receive_drain_work_us: MetricSamples::with_salt(salt ^ 7),
            playout_pull_work_us: MetricSamples::with_salt(salt ^ 8),
            stress_playout_pull_work_us: MetricSamples::with_salt(salt ^ 9),
            interest_entry_to_first_media_us: MetricSamples::with_salt(salt ^ 10),
            talkspurt_start_to_audio_us: MetricSamples::with_salt(salt ^ 11),
            ..Self::default()
        }
    }

    fn merge(&mut self, other: Self) {
        self.received_datagrams += other.received_datagrams;
        self.accepted_datagrams += other.accepted_datagrams;
        self.outside_interest_datagrams += other.outside_interest_datagrams;
        self.malformed_datagrams += other.malformed_datagrams;
        self.received_bytes += other.received_bytes;
        self.media_impairment_attempted_datagrams += other.media_impairment_attempted_datagrams;
        self.media_impairment_delivered_datagrams += other.media_impairment_delivered_datagrams;
        self.media_impairment_dropped_datagrams += other.media_impairment_dropped_datagrams;
        self.latencies_us.merge(other.latencies_us);
        self.queue_delays_us.merge(other.queue_delays_us);
        self.stress_queue_delays_us
            .merge(other.stress_queue_delays_us);
        self.listener_callback_work_us
            .merge(other.listener_callback_work_us);
        self.stress_listener_callback_work_us
            .merge(other.stress_listener_callback_work_us);
        self.receive_drain_work_us
            .merge(other.receive_drain_work_us);
        self.stress_receive_drain_work_us
            .merge(other.stress_receive_drain_work_us);
        self.playout_pull_work_us.merge(other.playout_pull_work_us);
        self.stress_playout_pull_work_us
            .merge(other.stress_playout_pull_work_us);
        self.interest_entry_to_first_media_us
            .merge(other.interest_entry_to_first_media_us);
        self.talkspurt_start_to_audio_us
            .merge(other.talkspurt_start_to_audio_us);
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
        self.affected_route_max_transport_gap_us = self
            .affected_route_max_transport_gap_us
            .max(other.affected_route_max_transport_gap_us);
        self.unaffected_route_max_transport_gap_us = self
            .unaffected_route_max_transport_gap_us
            .max(other.unaffected_route_max_transport_gap_us);
        self.target_delay_observations += other.target_delay_observations;
        self.target_delay_ge_100_ms_observations += other.target_delay_ge_100_ms_observations;
        self.target_delay_ge_150_ms_observations += other.target_delay_ge_150_ms_observations;
        self.target_delay_ge_100_ms_max_continuous_ticks = self
            .target_delay_ge_100_ms_max_continuous_ticks
            .max(other.target_delay_ge_100_ms_max_continuous_ticks);
        self.target_delay_ge_150_ms_max_continuous_ticks = self
            .target_delay_ge_150_ms_max_continuous_ticks
            .max(other.target_delay_ge_150_ms_max_continuous_ticks);
        self.neteq_max_current_buffer_ms = self
            .neteq_max_current_buffer_ms
            .max(other.neteq_max_current_buffer_ms);
        self.neteq_max_target_delay_ms = self
            .neteq_max_target_delay_ms
            .max(other.neteq_max_target_delay_ms);
    }
}

impl SendCounters {
    fn with_salt(salt: u64) -> Self {
        Self {
            fanout_spans_us: MetricSamples::with_salt(salt ^ 1),
            stress_fanout_spans_us: MetricSamples::with_salt(salt ^ 2),
            callback_work_us: MetricSamples::with_salt(salt ^ 3),
            stress_callback_work_us: MetricSamples::with_salt(salt ^ 4),
            ..Self::default()
        }
    }

    fn merge(&mut self, other: Self) {
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
        self.fanout_spans_us.merge(other.fanout_spans_us);
        self.stress_fanout_spans_us
            .merge(other.stress_fanout_spans_us);
        self.callback_work_us.merge(other.callback_work_us);
        self.stress_callback_work_us
            .merge(other.stress_callback_work_us);
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
         \x20 --topology NAME  direct or star (default direct)\n\
         \x20 --participants N  Fully connected participants (2-64; default 4)\n\
         \x20 --talkers N       Scheduled talkers (default 1)\n\
         \x20 --seconds N       Media duration, at least 0.5 (default 5)\n\
         \x20 --dtx on|off      Opus discontinuous transmission (default on)\n\
         \x20 --scenario NAME   baseline or game-interest (default baseline)\n\
         \x20 --delivery NAME   sender-filtered or broadcast-discard\n\
         \x20 --receiver-policy NAME  retire or pool (default retire)\n\
         \x20 --interest-profile NAME rotating, crowd-burst, group-merge, or boundary-oscillation\n\
         \x20 --interest-listeners N  Interested listeners per game talker (default 7)\n\
         \x20 --media-impairment NAME  none, uniform-loss, burst-loss, or outage\n\
         \x20 --media-loss-percent P  Endpoint-boundary loss percentage (default 3)\n\
         \x20 --media-burst-ms N      Mean burst duration for burst-loss (default 60)\n\
         \x20 --media-outage-start-ms N  Outage start on media timeline (default 3000)\n\
         \x20 --media-outage-duration-ms N  Outage length (default 300)\n\
         \x20 --churn NAME      none, join, leave, reconnect, or replace (default none)\n\
         \x20 --churn-participant N  Participant to disconnect (default 0)\n\
         \x20 --churn-start-ms N  Disconnect time on media timeline (default 6000)\n\
         \x20 --churn-downtime-ms N  Time before reconnecting (default 1000)\n\
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
    let mesh = build_mesh(&config, clock_start, receive_txs).await?;

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
    let churn_task = if config.churn_profile.active() {
        Some(tokio::spawn(run_churn(
            mesh.endpoints.clone(),
            mesh.connections.clone(),
            mesh.receive_txs.clone(),
            Arc::clone(&mesh.readers),
            media_start,
            clock_start,
            config.clone(),
        )))
    } else {
        None
    };
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
        rss_samples.push(rss_sample(media_start)?);
        next_rss_sample += Duration::from_secs(10);
    }
    tokio::time::sleep_until(tokio::time::Instant::from_std(media_end)).await;
    let current_rss_kib_after_media = current_rss_kib()?;
    let allocator = allocator_usage();
    rss_samples.push(RssSample {
        elapsed_seconds: media_start.elapsed().as_secs_f64(),
        current_rss_kib: current_rss_kib_after_media,
        allocator_arena_kib: allocator.arena_kib,
        allocator_in_use_kib: allocator.in_use_kib,
        allocator_free_kib: allocator.free_kib,
        allocator_mmap_kib: allocator.mmap_kib,
    });
    let media_wall = media_start.elapsed();
    let after_media = process_usage()?;
    let mut send_counters = SendCounters::default();
    for task in sender_tasks {
        send_counters.merge(task.await.context("media sender task panicked")??);
    }
    let churn_result = if let Some(task) = churn_task {
        task.await.context("churn task panicked")??
    } else {
        ChurnResult::default()
    };
    let mut counters = RunCounters::default();
    let mut concealed_samples = 0_u64;
    let mut concealment_events = 0_u64;
    let mut silent_concealed_samples = 0_u64;
    let mut late_packets_discarded = 0_u64;
    let mut inserted_samples_for_deceleration = 0_u64;
    let mut removed_samples_for_acceleration = 0_u64;
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
    let mut neteq_timeline = Vec::new();
    for task in listener_tasks {
        let mut listener = task.await.context("listener task panicked")??;
        participants_metrics.push(participant_metrics(&mut listener));
        neteq_timeline.append(&mut listener.neteq_timeline);
        active_receiver_count += listener.active_receiver_count;
        receiver_creations += listener.receiver_creations;
        receiver_reuses += listener.receiver_reuses;
        receiver_retirements += listener.receiver_retirements;
        max_concurrent_receivers = max_concurrent_receivers.max(listener.max_concurrent_receivers);
        max_receiver_pool = max_receiver_pool.max(listener.max_receiver_pool);
        counters.merge(listener.counters);
        concealed_samples = concealed_samples.saturating_add(listener.concealed_samples);
        concealment_events = concealment_events.saturating_add(listener.concealment_events);
        silent_concealed_samples =
            silent_concealed_samples.saturating_add(listener.silent_concealed_samples);
        late_packets_discarded =
            late_packets_discarded.saturating_add(listener.late_packets_discarded);
        inserted_samples_for_deceleration = inserted_samples_for_deceleration
            .saturating_add(listener.inserted_samples_for_deceleration);
        removed_samples_for_acceleration = removed_samples_for_acceleration
            .saturating_add(listener.removed_samples_for_acceleration);
        receiver_errors += listener.receiver_errors;
        max_current_buffer_ms = max_current_buffer_ms.max(listener.max_current_buffer_ms);
        max_target_delay_ms = max_target_delay_ms.max(listener.max_target_delay_ms);
    }
    neteq_timeline.sort_by(|left, right| {
        left.participant
            .cmp(&right.participant)
            .then_with(|| left.elapsed_seconds.total_cmp(&right.elapsed_seconds))
    });

    let cpu_seconds = after_media.cpu_seconds - after_setup.cpu_seconds;
    let media_seconds = media_wall.as_secs_f64();
    let sfu = mesh.sfu_counters.snapshot();
    let expected_received_datagrams = match config.topology {
        Topology::Direct => send_counters.sent_datagrams,
        Topology::Star => sfu.forwarded_datagrams,
    };
    let missing_datagrams = expected_received_datagrams.saturating_sub(counters.received_datagrams);
    let metrics = Metrics {
        schema_version: 10,
        metric_sample_capacity: METRIC_SAMPLE_CAPACITY,
        topology: config.topology.as_str(),
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
        media_impairment: config.media_impairment.as_str(),
        media_loss_percent: config.media_loss_percent,
        media_burst_ms: config.media_burst_ms,
        media_outage_start_ms: config.media_outage_start_ms,
        media_outage_duration_ms: config.media_outage_duration_ms,
        churn_profile: config.churn_profile.as_str(),
        churn_participant: config.churn_participant,
        churn_start_ms: config.churn_start_ms,
        churn_downtime_ms: config.churn_downtime_ms,
        churn_disconnects: churn_result.disconnects,
        churn_reconnects: churn_result.reconnects,
        churn_reconnect_errors: churn_result.reconnect_errors,
        churn_reconnect_duration_ms: churn_result.reconnect_duration_ms,
        churn_new_identity: churn_result.new_identity,
        affected_route_max_transport_gap_ms: counters.affected_route_max_transport_gap_us as f64
            / 1_000.0,
        unaffected_route_max_transport_gap_ms: counters.unaffected_route_max_transport_gap_us
            as f64
            / 1_000.0,
        seed: config.seed,
        runtime_worker_threads: config.runtime_workers,
        opus_version: voice_core::opus_version(),
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
        mesh_connections: match config.topology {
            Topology::Star => config.participants,
            Topology::Direct if config.churn_profile == ChurnProfile::Join => {
                (config.participants - 1) * (config.participants - 2) / 2
            }
            Topology::Direct => config.participants * (config.participants - 1) / 2,
        },
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
        neteq_timeline,
        sender_ticks: send_counters.sender_ticks,
        sender_callbacks: send_counters.sender_callbacks,
        sender_skipped_ticks: send_counters.sender_skipped_ticks,
        stress_events: send_counters.stress_events,
        stress_sender_ticks: send_counters.stress_sender_ticks,
        stress_sender_skipped_ticks: send_counters.stress_sender_skipped_ticks,
        stress_sent_datagrams: send_counters.stress_sent_datagrams,
        stress_fanout_span_us_p95: send_counters.stress_fanout_spans_us.percentile(95),
        sender_callback_work_us_p95: send_counters.callback_work_us.percentile(95),
        stress_sender_callback_work_us_p95: send_counters.stress_callback_work_us.percentile(95),
        fanout_span_us_p50: send_counters.fanout_spans_us.percentile(50),
        fanout_span_us_p95: send_counters.fanout_spans_us.percentile(95),
        fanout_span_us_max: send_counters.fanout_spans_us.max(),
        encoded_packets: send_counters.encoded_packets,
        sent_datagrams: send_counters.sent_datagrams,
        send_errors: send_counters.send_errors,
        received_datagrams: counters.received_datagrams,
        accepted_datagrams: counters.accepted_datagrams,
        outside_interest_datagrams: counters.outside_interest_datagrams,
        media_impairment_attempted_datagrams: counters.media_impairment_attempted_datagrams,
        media_impairment_delivered_datagrams: counters.media_impairment_delivered_datagrams,
        media_impairment_dropped_datagrams: counters.media_impairment_dropped_datagrams,
        malformed_datagrams: counters.malformed_datagrams,
        missing_datagrams,
        sent_bytes: send_counters.sent_bytes,
        received_bytes: counters.received_bytes,
        outbound_mbit_per_second: if media_seconds > 0.0 {
            send_counters.sent_bytes as f64 * 8.0 / media_seconds / 1_000_000.0
        } else {
            0.0
        },
        sfu_received_datagrams: sfu.received_datagrams,
        sfu_received_bytes: sfu.received_bytes,
        sfu_forwarded_datagrams: sfu.forwarded_datagrams,
        sfu_forwarded_bytes: sfu.forwarded_bytes,
        sfu_send_errors: sfu.send_errors,
        sfu_outbound_mbit_per_second: if media_seconds > 0.0 {
            sfu.forwarded_bytes as f64 * 8.0 / media_seconds / 1_000_000.0
        } else {
            0.0
        },
        latency_us_p50: counters.latencies_us.percentile(50),
        latency_us_p95: counters.latencies_us.percentile(95),
        latency_us_p99: counters.latencies_us.percentile(99),
        latency_us_max: counters.latencies_us.max(),
        receive_queue_delay_us_p95: counters.queue_delays_us.percentile(95),
        receive_queue_delay_us_max: counters.queue_delays_us.max(),
        interest_entry_to_first_media_us_p50: counters
            .interest_entry_to_first_media_us
            .percentile(50),
        interest_entry_to_first_media_us_p95: counters
            .interest_entry_to_first_media_us
            .percentile(95),
        interest_entry_to_first_media_us_max: counters.interest_entry_to_first_media_us.max(),
        interest_entry_events: counters.interest_entry_to_first_media_us.count() as usize,
        talkspurt_start_to_audio_us_p50: counters.talkspurt_start_to_audio_us.percentile(50),
        talkspurt_start_to_audio_us_p95: counters.talkspurt_start_to_audio_us.percentile(95),
        talkspurt_start_to_audio_us_max: counters.talkspurt_start_to_audio_us.max(),
        talkspurt_audio_events: counters.talkspurt_start_to_audio_us.count() as usize,
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
        stress_receive_queue_delay_us_p95: counters.stress_queue_delays_us.percentile(95),
        listener_callback_work_us_p95: counters.listener_callback_work_us.percentile(95),
        stress_listener_callback_work_us_p95: counters
            .stress_listener_callback_work_us
            .percentile(95),
        receive_drain_work_us_p95: counters.receive_drain_work_us.percentile(95),
        stress_receive_drain_work_us_p95: counters.stress_receive_drain_work_us.percentile(95),
        playout_pull_work_us_p95: counters.playout_pull_work_us.percentile(95),
        stress_playout_pull_work_us_p95: counters.stress_playout_pull_work_us.percentile(95),
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
        neteq_concealment_events: concealment_events,
        neteq_silent_concealed_samples: silent_concealed_samples,
        neteq_late_packets_discarded: late_packets_discarded,
        neteq_inserted_samples_for_deceleration: inserted_samples_for_deceleration,
        neteq_removed_samples_for_acceleration: removed_samples_for_acceleration,
        neteq_receiver_errors: receiver_errors,
        neteq_max_current_buffer_ms: max_current_buffer_ms,
        neteq_max_target_delay_ms: max_target_delay_ms,
        neteq_target_delay_observations: counters.target_delay_observations,
        neteq_target_delay_ge_100_ms_percent: percent(
            counters.target_delay_ge_100_ms_observations,
            counters.target_delay_observations,
        ),
        neteq_target_delay_ge_150_ms_percent: percent(
            counters.target_delay_ge_150_ms_observations,
            counters.target_delay_observations,
        ),
        neteq_target_delay_ge_100_ms_max_continuous_ms: counters
            .target_delay_ge_100_ms_max_continuous_ticks
            * PLAYOUT_TICK.as_millis() as u64,
        neteq_target_delay_ge_150_ms_max_continuous_ms: counters
            .target_delay_ge_150_ms_max_continuous_ticks
            * PLAYOUT_TICK.as_millis() as u64,
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
    config: &Config,
    clock_start: Instant,
    receive_txs: Vec<mpsc::UnboundedSender<ReceivedDatagram>>,
) -> Result<Mesh> {
    match config.topology {
        Topology::Direct => build_direct_mesh(config, clock_start, receive_txs).await,
        Topology::Star => build_star(config, clock_start, receive_txs).await,
    }
}

async fn build_direct_mesh(
    config: &Config,
    clock_start: Instant,
    receive_txs: Vec<mpsc::UnboundedSender<ReceivedDatagram>>,
) -> Result<Mesh> {
    let participants = config.participants;
    let mut endpoints = Vec::with_capacity(participants);
    for participant in 0..participants {
        let endpoint = if config.churn_profile == ChurnProfile::Join
            && participant == config.churn_participant
        {
            None
        } else {
            Some(new_endpoint().await?)
        };
        endpoints.push(Arc::new(RwLock::new(endpoint)));
    }

    let connections = (0..participants)
        .map(|_| {
            (0..participants)
                .map(|_| Arc::new(RwLock::new(None)))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for connector in 0..participants {
        for acceptor in (connector + 1)..participants {
            let accepting_endpoint = endpoint_from_slot(&endpoints[acceptor]);
            let connecting_endpoint = endpoint_from_slot(&endpoints[connector]);
            let (Some(connecting_endpoint), Some(accepting_endpoint)) =
                (connecting_endpoint, accepting_endpoint)
            else {
                continue;
            };
            let accepting_addr = accepting_endpoint.addr();
            let accept_endpoint = accepting_endpoint.clone();
            let accepting = async move {
                let incoming = accept_endpoint
                    .accept()
                    .await
                    .context("endpoint closed while building mesh")?;
                incoming.await.context("accept mesh connection")
            };
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
            *connections[connector][acceptor]
                .write()
                .expect("connection lock poisoned") = Some(outbound);
            *connections[acceptor][connector]
                .write()
                .expect("connection lock poisoned") = Some(inbound);
        }
    }

    let readers = Arc::new(Mutex::new(Vec::with_capacity(
        participants * (participants - 1),
    )));
    for (listener, listener_connections) in connections.iter().enumerate() {
        for (speaker, slot) in listener_connections.iter().enumerate() {
            let Some(connection) = slot.read().expect("connection lock poisoned").clone() else {
                continue;
            };
            spawn_datagram_reader(
                connection,
                speaker,
                receive_txs[listener].clone(),
                clock_start,
                &readers,
            );
        }
    }

    Ok(Mesh {
        endpoints,
        sfu_endpoint: None,
        connections,
        receive_txs,
        readers,
        sfu_counters: Arc::new(SfuCounters::default()),
    })
}

async fn build_star(
    config: &Config,
    clock_start: Instant,
    receive_txs: Vec<mpsc::UnboundedSender<ReceivedDatagram>>,
) -> Result<Mesh> {
    let participants = config.participants;
    let sfu_endpoint = new_endpoint().await?;
    let sfu_addr = sfu_endpoint.addr();
    let mut endpoints = Vec::with_capacity(participants);
    let connections = (0..participants)
        .map(|_| {
            (0..participants)
                .map(|_| Arc::new(RwLock::new(None)))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut sfu_connections = Vec::with_capacity(participants);

    for (participant, connection_row) in connections.iter().enumerate() {
        let endpoint = new_endpoint().await?;
        let accepting_endpoint = sfu_endpoint.clone();
        let accepting = async move {
            let incoming = accepting_endpoint
                .accept()
                .await
                .context("SFU endpoint closed while building star")?;
            incoming.await.context("accept star client")
        };
        let connecting_endpoint = endpoint.clone();
        let connecting_addr = sfu_addr.clone();
        let connecting = async move {
            connecting_endpoint
                .connect(connecting_addr, ALPN)
                .await
                .context("connect client to SFU")
        };
        let (client, server) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::try_join!(connecting, accepting)
        })
        .await
        .context("timed out building star connection")??;
        *connection_row[participant]
            .write()
            .expect("connection lock poisoned") = Some(client);
        endpoints.push(Arc::new(RwLock::new(Some(endpoint))));
        sfu_connections.push(server);
    }

    let readers = Arc::new(Mutex::new(Vec::with_capacity(participants * 2)));
    for (participant, connection_row) in connections.iter().enumerate() {
        let connection = connection_row[participant]
            .read()
            .expect("connection lock poisoned")
            .clone()
            .expect("star client connection was installed");
        spawn_star_client_reader(
            connection,
            receive_txs[participant].clone(),
            clock_start,
            &readers,
        );
    }
    let sfu_counters = Arc::new(SfuCounters::default());
    let sfu_connections = Arc::new(sfu_connections);
    for speaker in 0..participants {
        spawn_sfu_reader(
            speaker,
            sfu_connections[speaker].clone(),
            Arc::clone(&sfu_connections),
            config.clone(),
            Arc::clone(&sfu_counters),
            &readers,
        );
    }

    Ok(Mesh {
        endpoints,
        sfu_endpoint: Some(sfu_endpoint),
        connections,
        receive_txs,
        readers,
        sfu_counters,
    })
}

async fn new_endpoint() -> Result<Endpoint> {
    Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .alpns(vec![ALPN.to_vec()])
        .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .context("bind benchmark endpoint to loopback")?
        .bind()
        .await
        .context("start benchmark endpoint")
}

fn endpoint_from_slot(slot: &EndpointSlot) -> Option<Endpoint> {
    slot.read().expect("endpoint lock poisoned").clone()
}

fn spawn_datagram_reader(
    connection: Connection,
    speaker: usize,
    tx: mpsc::UnboundedSender<ReceivedDatagram>,
    clock_start: Instant,
    readers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    let reader = tokio::spawn(async move {
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
    });
    readers.lock().expect("reader lock poisoned").push(reader);
}

fn spawn_star_client_reader(
    connection: Connection,
    tx: mpsc::UnboundedSender<ReceivedDatagram>,
    clock_start: Instant,
    readers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    let reader = tokio::spawn(async move {
        while let Ok(bytes) = connection.read_datagram().await {
            let Some(speaker) = envelope_speaker(&bytes) else {
                continue;
            };
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
    });
    readers.lock().expect("reader lock poisoned").push(reader);
}

fn spawn_sfu_reader(
    speaker: usize,
    source: Connection,
    clients: Arc<Vec<Connection>>,
    config: Config,
    counters: Arc<SfuCounters>,
    readers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    let reader = tokio::spawn(async move {
        while let Ok(bytes) = source.read_datagram().await {
            counters.received_datagrams.fetch_add(1, Ordering::Relaxed);
            counters
                .received_bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            if envelope_speaker(&bytes) != Some(speaker) || bytes.len() < ENVELOPE_HEADER_LEN {
                continue;
            }
            let frame_index = u64::from_be_bytes(
                bytes[8..16]
                    .try_into()
                    .expect("checked SFU envelope length"),
            );
            for (listener, connection) in clients.iter().enumerate() {
                if listener == speaker {
                    continue;
                }
                if config.scenario == Scenario::GameInterest
                    && config.delivery == Delivery::SenderFiltered
                    && !listener_interested(&config, frame_index, speaker, listener)
                {
                    continue;
                }
                match connection.send_datagram(bytes.clone()) {
                    Ok(()) => {
                        counters.forwarded_datagrams.fetch_add(1, Ordering::Relaxed);
                        counters
                            .forwarded_bytes
                            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    }
                    Err(_) => {
                        counters.send_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    });
    readers.lock().expect("reader lock poisoned").push(reader);
}

fn envelope_speaker(bytes: &[u8]) -> Option<usize> {
    let raw = u64::from_be_bytes(bytes.get(16..24)?.try_into().ok()?);
    usize::try_from(raw).ok()
}

async fn run_churn(
    endpoints: Vec<EndpointSlot>,
    connections: Vec<Vec<ConnectionSlot>>,
    receive_txs: Vec<mpsc::UnboundedSender<ReceivedDatagram>>,
    readers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    media_start: Instant,
    clock_start: Instant,
    config: Config,
) -> Result<ChurnResult> {
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        media_start + Duration::from_millis(config.churn_start_ms),
    ))
    .await;

    let participant = config.churn_participant;
    let mut result = ChurnResult::default();
    if config.churn_profile != ChurnProfile::Join {
        for (peer, peer_connections) in connections.iter().enumerate().take(config.participants) {
            if peer == participant {
                continue;
            }
            let local = connections[participant][peer]
                .write()
                .expect("connection lock poisoned")
                .take();
            let remote = peer_connections[participant]
                .write()
                .expect("connection lock poisoned")
                .take();
            if let Some(connection) = local.or(remote) {
                connection.close(0u8.into(), b"benchmark churn");
                result.disconnects += 1;
            }
        }
    }

    if config.churn_profile == ChurnProfile::Leave {
        let endpoint = endpoints[participant]
            .write()
            .expect("endpoint lock poisoned")
            .take();
        if let Some(endpoint) = endpoint {
            endpoint.close().await;
        }
        return Ok(result);
    }

    if config.churn_profile == ChurnProfile::Replace {
        let old_endpoint = endpoints[participant]
            .write()
            .expect("endpoint lock poisoned")
            .take();
        if let Some(endpoint) = old_endpoint {
            endpoint.close().await;
        }
    }
    if matches!(
        config.churn_profile,
        ChurnProfile::Reconnect | ChurnProfile::Replace
    ) {
        tokio::time::sleep(Duration::from_millis(config.churn_downtime_ms)).await;
    }
    if matches!(
        config.churn_profile,
        ChurnProfile::Join | ChurnProfile::Replace
    ) {
        let endpoint = new_endpoint().await?;
        *endpoints[participant]
            .write()
            .expect("endpoint lock poisoned") = Some(endpoint);
        result.new_identity = true;
    }

    let reconnect_start = Instant::now();
    if !config.churn_profile.reconnects() {
        return Ok(result);
    }
    for (peer, peer_connections) in connections.iter().enumerate().take(config.participants) {
        if peer == participant {
            continue;
        }
        let accepting_endpoint = endpoint_from_slot(&endpoints[peer])
            .context("peer endpoint missing while reconnecting")?;
        let connecting_endpoint = endpoint_from_slot(&endpoints[participant])
            .context("churn participant endpoint missing while reconnecting")?;
        let accepting_addr = accepting_endpoint.addr();
        let accept_endpoint = accepting_endpoint.clone();
        let accepting = async move {
            let incoming = accept_endpoint
                .accept()
                .await
                .context("endpoint closed while reconnecting")?;
            incoming.await.context("accept reconnected peer")
        };
        let connecting = async move {
            connecting_endpoint
                .connect(accepting_addr, ALPN)
                .await
                .context("reconnect churn participant")
        };
        let pair = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::try_join!(connecting, accepting)
        })
        .await;
        let (local, remote) = match pair {
            Ok(Ok(pair)) => pair,
            Ok(Err(_)) | Err(_) => {
                result.reconnect_errors += 1;
                continue;
            }
        };
        *connections[participant][peer]
            .write()
            .expect("connection lock poisoned") = Some(local.clone());
        *peer_connections[participant]
            .write()
            .expect("connection lock poisoned") = Some(remote.clone());
        spawn_datagram_reader(
            local,
            peer,
            receive_txs[participant].clone(),
            clock_start,
            &readers,
        );
        spawn_datagram_reader(
            remote,
            participant,
            receive_txs[peer].clone(),
            clock_start,
            &readers,
        );
        result.reconnects += 1;
    }
    result.reconnect_duration_ms = reconnect_start.elapsed().as_secs_f64() * 1_000.0;
    Ok(result)
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
        reported_concealment_events: 0,
        reported_silent_concealed_samples: 0,
        reported_late_packets_discarded: 0,
        reported_inserted_samples_for_deceleration: 0,
        reported_removed_samples_for_acceleration: 0,
        reported_error: false,
        target_delay_ge_100_ms_continuous_ticks: 0,
        target_delay_ge_150_ms_continuous_ticks: 0,
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
    totals.concealment_events = totals.concealment_events.saturating_add(
        stats
            .concealment_events
            .saturating_sub(slot.reported_concealment_events),
    );
    slot.reported_concealment_events = stats.concealment_events;
    totals.silent_concealed_samples = totals.silent_concealed_samples.saturating_add(
        stats
            .silent_concealed_samples
            .saturating_sub(slot.reported_silent_concealed_samples),
    );
    slot.reported_silent_concealed_samples = stats.silent_concealed_samples;
    totals.late_packets_discarded = totals.late_packets_discarded.saturating_add(
        stats
            .late_packets_discarded
            .saturating_sub(slot.reported_late_packets_discarded),
    );
    slot.reported_late_packets_discarded = stats.late_packets_discarded;
    totals.inserted_samples_for_deceleration =
        totals.inserted_samples_for_deceleration.saturating_add(
            stats
                .inserted_samples_for_deceleration
                .saturating_sub(slot.reported_inserted_samples_for_deceleration),
        );
    slot.reported_inserted_samples_for_deceleration = stats.inserted_samples_for_deceleration;
    totals.removed_samples_for_acceleration =
        totals.removed_samples_for_acceleration.saturating_add(
            stats
                .removed_samples_for_acceleration
                .saturating_sub(slot.reported_removed_samples_for_acceleration),
        );
    slot.reported_removed_samples_for_acceleration = stats.removed_samples_for_acceleration;
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
    let mut counters = RunCounters::with_salt(listener as u64);
    let mut interest_active = vec![false; receivers.len()];
    let mut interest_started_us = vec![None; receivers.len()];
    let mut receiver_creations = match config.scenario {
        Scenario::Baseline => receivers.iter().flatten().count() as u64,
        Scenario::GameInterest => 0,
    };
    let mut receiver_retirements = 0_u64;
    let mut receiver_reuses = 0_u64;
    let mut receiver_pool = Vec::new();
    let mut impairment_routes = (0..receivers.len())
        .map(|speaker| ImpairmentRouteState::new(config.seed, speaker, listener))
        .collect::<Vec<_>>();
    let mut last_transport_received_us = vec![None; receivers.len()];
    if config.churn_profile == ChurnProfile::Join {
        let media_start_us = media_start.duration_since(clock_start).as_micros() as u64;
        for (speaker, last_received) in last_transport_received_us.iter_mut().enumerate() {
            if route_affected_by_churn(listener, speaker, &config) {
                *last_received = Some(media_start_us);
            }
        }
    }
    let mut receiver_totals = ReceiverTotals::default();
    let mut max_concurrent_receivers = receivers.iter().flatten().count();
    let mut max_receiver_pool = 0_usize;
    let mut neteq_timeline = Vec::new();
    let mut next_neteq_sample = Duration::ZERO;

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
            &mut impairment_routes,
            &mut last_transport_received_us,
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
        let media_elapsed = media_start.elapsed();
        if media_elapsed >= next_neteq_sample
            && neteq_timeline.len() < NETEQ_TIMELINE_CAPACITY_PER_PARTICIPANT
        {
            neteq_timeline.push(neteq_timeline_sample(
                listener,
                media_elapsed,
                &receivers,
                &receiver_pool,
                &receiver_totals,
            ));
            next_neteq_sample += Duration::from_secs(1);
        }
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
        &mut impairment_routes,
        &mut last_transport_received_us,
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
        neteq_timeline,
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
            &mut impairment_routes,
            &mut last_transport_received_us,
        )?;
    }
    if config.churn_profile.active() {
        let media_end_us = media_end.duration_since(clock_start).as_micros() as u64;
        for (speaker, last_received) in last_transport_received_us.iter().enumerate() {
            if route_affected_by_churn(listener, speaker, &config) {
                if let Some(last_received_us) = last_received {
                    result.counters.affected_route_max_transport_gap_us = result
                        .counters
                        .affected_route_max_transport_gap_us
                        .max(media_end_us.saturating_sub(*last_received_us));
                }
            }
        }
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
    result.concealment_events = receiver_totals.concealment_events;
    result.silent_concealed_samples = receiver_totals.silent_concealed_samples;
    result.late_packets_discarded = receiver_totals.late_packets_discarded;
    result.inserted_samples_for_deceleration = receiver_totals.inserted_samples_for_deceleration;
    result.removed_samples_for_acceleration = receiver_totals.removed_samples_for_acceleration;
    result.receiver_errors = receiver_totals.receiver_errors;
    result.max_current_buffer_ms = receiver_totals
        .max_current_buffer_ms
        .max(result.counters.neteq_max_current_buffer_ms);
    result.max_target_delay_ms = receiver_totals
        .max_target_delay_ms
        .max(result.counters.neteq_max_target_delay_ms);
    Ok(result)
}

#[allow(clippy::too_many_arguments)] // Keep each independent sender's clock and transport explicit.
async fn send_media(
    speaker: usize,
    sender_count: usize,
    duration: Duration,
    connections: Vec<ConnectionSlot>,
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
    let mut counters = SendCounters::with_salt(speaker as u64);
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
    connections: &[ConnectionSlot],
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
    wire.extend_from_slice(&(speaker as u64).to_be_bytes());
    packet.encode_to_bytes(&mut wire);
    let wire = Bytes::from(wire);
    let fanout_start = Instant::now();
    if config.topology == Topology::Star {
        let connection = connections[speaker]
            .read()
            .expect("connection lock poisoned")
            .clone();
        if let Some(connection) = connection {
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
        } else {
            counters.send_errors += 1;
        }
        let fanout_span_us = fanout_start.elapsed().as_micros() as u64;
        counters.fanout_spans_us.push(fanout_span_us);
        if stress {
            counters.stress_fanout_spans_us.push(fanout_span_us);
        }
        return Ok(());
    }
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
        let connection = connection.read().expect("connection lock poisoned").clone();
        let Some(connection) = connection else {
            counters.send_errors += 1;
            continue;
        };
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
    impairment_routes: &mut [ImpairmentRouteState],
    last_transport_received_us: &mut [Option<u64>],
) -> Result<()> {
    while let Ok(datagram) = receive_rx.try_recv() {
        counters.received_datagrams += 1;
        counters.received_bytes += datagram.bytes.len() as u64;
        let last_received = last_transport_received_us
            .get_mut(datagram.speaker)
            .context("received speaker outside transport gap table")?;
        if let Some(previous_us) = last_received.replace(datagram.received_at_us) {
            let gap_us = datagram.received_at_us.saturating_sub(previous_us);
            if route_affected_by_churn(listener, datagram.speaker, config) {
                counters.affected_route_max_transport_gap_us =
                    counters.affected_route_max_transport_gap_us.max(gap_us);
            } else {
                counters.unaffected_route_max_transport_gap_us =
                    counters.unaffected_route_max_transport_gap_us.max(gap_us);
            }
        }
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
            datagram.bytes[8..16]
                .try_into()
                .expect("checked envelope length"),
        );
        counters.media_impairment_attempted_datagrams += 1;
        let impairment = impairment_routes
            .get_mut(datagram.speaker)
            .context("received speaker outside impairment route table")?;
        if impairment.should_drop(config, frame_index) {
            counters.media_impairment_dropped_datagrams += 1;
            continue;
        }
        counters.media_impairment_delivered_datagrams += 1;
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

fn route_affected_by_churn(listener: usize, speaker: usize, config: &Config) -> bool {
    speaker != listener
        && config.churn_profile.active()
        && (listener == config.churn_participant || speaker == config.churn_participant)
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
        let stats = slot.receiver.stats();
        counters.neteq_max_current_buffer_ms = counters
            .neteq_max_current_buffer_ms
            .max(stats.current_buffer_size_ms);
        counters.neteq_max_target_delay_ms = counters
            .neteq_max_target_delay_ms
            .max(stats.target_delay_ms);
        if stats.intentional_silence {
            slot.target_delay_ge_100_ms_continuous_ticks = 0;
            slot.target_delay_ge_150_ms_continuous_ticks = 0;
        } else {
            counters.target_delay_observations += 1;
            if stats.target_delay_ms >= TARGET_DELAY_NOTICEABLE_MS {
                counters.target_delay_ge_100_ms_observations += 1;
                slot.target_delay_ge_100_ms_continuous_ticks += 1;
                counters.target_delay_ge_100_ms_max_continuous_ticks = counters
                    .target_delay_ge_100_ms_max_continuous_ticks
                    .max(slot.target_delay_ge_100_ms_continuous_ticks);
            } else {
                slot.target_delay_ge_100_ms_continuous_ticks = 0;
            }
            if stats.target_delay_ms >= TARGET_DELAY_HIGH_MS {
                counters.target_delay_ge_150_ms_observations += 1;
                slot.target_delay_ge_150_ms_continuous_ticks += 1;
                counters.target_delay_ge_150_ms_max_continuous_ticks = counters
                    .target_delay_ge_150_ms_max_continuous_ticks
                    .max(slot.target_delay_ge_150_ms_continuous_ticks);
            } else {
                slot.target_delay_ge_150_ms_continuous_ticks = 0;
            }
        }
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

fn neteq_timeline_sample(
    participant: usize,
    elapsed: Duration,
    receivers: &[Option<ReceiverSlot>],
    receiver_pool: &[ReceiverSlot],
    totals: &ReceiverTotals,
) -> NetEqTimelineSample {
    let mut current_buffer_ms_max = 0;
    let mut target_delay_ms_max = 0;
    let mut concealed_samples = totals.concealed_samples;
    for slot in receivers.iter().flatten().chain(receiver_pool) {
        let stats = slot.receiver.stats();
        current_buffer_ms_max = current_buffer_ms_max.max(stats.current_buffer_size_ms);
        target_delay_ms_max = target_delay_ms_max.max(stats.target_delay_ms);
        concealed_samples = concealed_samples.saturating_add(
            stats
                .concealed_samples
                .saturating_sub(slot.reported_concealed_samples),
        );
    }
    NetEqTimelineSample {
        participant,
        elapsed_seconds: elapsed.as_secs_f64(),
        active_receivers: receivers.iter().flatten().count(),
        current_buffer_ms_max,
        target_delay_ms_max,
        concealed_samples,
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
                    retired.target_delay_ge_100_ms_continuous_ticks = 0;
                    retired.target_delay_ge_150_ms_continuous_ticks = 0;
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
        media_impairment_attempted_datagrams: listener
            .counters
            .media_impairment_attempted_datagrams,
        media_impairment_delivered_datagrams: listener
            .counters
            .media_impairment_delivered_datagrams,
        media_impairment_dropped_datagrams: listener.counters.media_impairment_dropped_datagrams,
        affected_route_max_transport_gap_ms: listener.counters.affected_route_max_transport_gap_us
            as f64
            / 1_000.0,
        unaffected_route_max_transport_gap_ms: listener
            .counters
            .unaffected_route_max_transport_gap_us
            as f64
            / 1_000.0,
        active_receiver_count: listener.active_receiver_count,
        receiver_creations: listener.receiver_creations,
        receiver_reuses: listener.receiver_reuses,
        receiver_retirements: listener.receiver_retirements,
        max_concurrent_receivers: listener.max_concurrent_receivers,
        max_receiver_pool: listener.max_receiver_pool,
        receive_queue_delay_us_p95: listener.counters.queue_delays_us.percentile(95),
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
        interest_entry_to_first_media_us_p95: listener
            .counters
            .interest_entry_to_first_media_us
            .percentile(95),
        interest_entry_events: listener.counters.interest_entry_to_first_media_us.count() as usize,
        talkspurt_start_to_audio_us_p95: listener
            .counters
            .talkspurt_start_to_audio_us
            .percentile(95),
        talkspurt_audio_events: listener.counters.talkspurt_start_to_audio_us.count() as usize,
        neteq_concealed_samples: listener.concealed_samples,
        neteq_concealment_events: listener.concealment_events,
        neteq_silent_concealed_samples: listener.silent_concealed_samples,
        neteq_late_packets_discarded: listener.late_packets_discarded,
        neteq_receiver_errors: listener.receiver_errors,
        neteq_target_delay_observations: listener.counters.target_delay_observations,
        neteq_target_delay_ge_100_ms_percent: percent(
            listener.counters.target_delay_ge_100_ms_observations,
            listener.counters.target_delay_observations,
        ),
        neteq_target_delay_ge_150_ms_percent: percent(
            listener.counters.target_delay_ge_150_ms_observations,
            listener.counters.target_delay_observations,
        ),
        neteq_target_delay_ge_100_ms_max_continuous_ms: listener
            .counters
            .target_delay_ge_100_ms_max_continuous_ticks
            * PLAYOUT_TICK.as_millis() as u64,
        neteq_target_delay_ge_150_ms_max_continuous_ms: listener
            .counters
            .target_delay_ge_150_ms_max_continuous_ticks
            * PLAYOUT_TICK.as_millis() as u64,
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

fn rss_sample(media_start: Instant) -> Result<RssSample> {
    let allocator = allocator_usage();
    Ok(RssSample {
        elapsed_seconds: media_start.elapsed().as_secs_f64(),
        current_rss_kib: current_rss_kib()?,
        allocator_arena_kib: allocator.arena_kib,
        allocator_in_use_kib: allocator.in_use_kib,
        allocator_free_kib: allocator.free_kib,
        allocator_mmap_kib: allocator.mmap_kib,
    })
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_usage() -> AllocatorUsage {
    // SAFETY: mallinfo2 takes no pointers and returns a snapshot of glibc's
    // process-global allocator counters.
    let info = unsafe { libc::mallinfo2() };
    AllocatorUsage {
        arena_kib: (info.arena / 1024) as u64,
        in_use_kib: (info.uordblks / 1024) as u64,
        free_kib: (info.fordblks / 1024) as u64,
        mmap_kib: (info.hblkhd / 1024) as u64,
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_usage() -> AllocatorUsage {
    AllocatorUsage::default()
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
    fn metric_samples_are_bounded_with_exact_count_and_max() {
        let mut samples = MetricSamples::with_salt(7);
        for value in 0..(METRIC_SAMPLE_CAPACITY as u64 * 3) {
            samples.push(value);
        }

        assert_eq!(samples.samples.len(), METRIC_SAMPLE_CAPACITY);
        assert_eq!(samples.count(), METRIC_SAMPLE_CAPACITY as u64 * 3);
        assert_eq!(samples.max(), METRIC_SAMPLE_CAPACITY as u64 * 3 - 1);
        assert!(samples.percentile(50) > METRIC_SAMPLE_CAPACITY as u64);
    }

    #[test]
    fn metric_sample_merge_remains_bounded() {
        let mut left = MetricSamples::with_salt(1);
        let mut right = MetricSamples::with_salt(2);
        for value in 0..10_000 {
            left.push(value);
            right.push(value + 10_000);
        }
        left.merge(right);

        assert_eq!(left.samples.len(), METRIC_SAMPLE_CAPACITY);
        assert_eq!(left.count(), 20_000);
        assert_eq!(left.max(), 19_999);
        assert!((9_000..=11_000).contains(&left.percentile(50)));
    }

    #[test]
    fn uniform_media_loss_is_deterministic_and_near_target() {
        let config = Config {
            media_impairment: MediaImpairment::UniformLoss,
            media_loss_percent: 3.0,
            ..Config::default()
        };
        let count_drops = || {
            let mut route = ImpairmentRouteState::new(9, 2, 5);
            (0..100_000)
                .filter(|frame| route.should_drop(&config, *frame))
                .count()
        };

        let drops = count_drops();
        assert_eq!(drops, count_drops());
        assert!((2_800..=3_200).contains(&drops));
    }

    #[test]
    fn burst_media_loss_clusters_drops_near_target() {
        let config = Config {
            media_impairment: MediaImpairment::BurstLoss,
            media_loss_percent: 5.0,
            media_burst_ms: 60,
            ..Config::default()
        };
        let mut route = ImpairmentRouteState::new(11, 3, 7);
        let drops = (0..100_000)
            .map(|frame| route.should_drop(&config, frame))
            .collect::<Vec<_>>();
        let drop_count = drops.iter().filter(|drop| **drop).count();
        let burst_count = drops
            .iter()
            .enumerate()
            .filter(|(index, drop)| **drop && (*index == 0 || !drops[*index - 1]))
            .count();

        assert!((4_500..=5_500).contains(&drop_count));
        assert!((2.5..=3.5).contains(&(drop_count as f64 / burst_count as f64)));
    }

    #[test]
    fn media_outage_uses_sender_timeline() {
        let config = Config {
            media_impairment: MediaImpairment::Outage,
            media_outage_start_ms: 100,
            media_outage_duration_ms: 60,
            ..Config::default()
        };
        let mut route = ImpairmentRouteState::new(1, 0, 1);

        let dropped = (0..10)
            .filter(|frame| route.should_drop(&config, *frame))
            .collect::<Vec<_>>();
        assert_eq!(dropped, vec![5, 6, 7]);
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

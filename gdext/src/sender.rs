use godot::builtin::{PackedByteArray, PackedFloat32Array, VarDictionary};
use godot::classes::{AudioServer, INode, Node};
use godot::obj::Singleton;
use godot::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use voice_core::{PacketFlags, VoiceEncoder, VoiceEncoderConfig};

use crate::packet_bytes::encode_packet_bytes;
use crate::stream::{AudioStreamNetwork, LoopbackTarget};

/// Called directly from the encode thread with encoded packet bytes.
/// When installed, bypasses the `encoded_packets` queue and `_process()` drain entirely.
pub(crate) type DirectSendHandler = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

const DEFAULT_MICROPHONE_FRAME_BUDGET: i32 = 960;
/// Target inter-packet interval for paced emission, in microseconds.
const PACE_INTERVAL_US: u64 = 20_000;
/// Maximum packets held in the outbound queue before dropping. With 20 ms
/// frames and 20 ms pacing this stays near 0–2 under normal conditions.
const MAX_PENDING_PACKETS: usize = 16;
/// Raw PCM backlog cap before the sender drops oldest samples.
const MAX_PENDING_PCM_SAMPLES: usize = 48_000 * 2;

#[derive(Debug, Default)]
struct WorkerQueues {
    pcm_samples: VecDeque<f32>,
    encoded_packets: VecDeque<Vec<u8>>,
}

#[derive(Debug, Default)]
struct WorkerStats {
    worker_ticks: AtomicI64,
    worker_ticks_with_pcm: AtomicI64,
    worker_empty_pcm_ticks: AtomicI64,
    worker_partial_pcm_ticks: AtomicI64,
    worker_ticks_with_packets: AtomicI64,
    worker_ticks_without_packets: AtomicI64,
    silent_ticks: AtomicI64,
    packets_emitted: AtomicI64,
    packets_dropped: AtomicI64,
    pcm_samples_dropped: AtomicI64,
    queued_pcm_samples: AtomicI64,
    queued_packets: AtomicI64,
    tick_lag_count: AtomicI64,
    tick_lag_sum_us: AtomicU64,
    tick_lag_max_us: AtomicU64,
    last_packet_emit_us: AtomicU64,
    packet_interval_count: AtomicI64,
    packet_interval_sum_us: AtomicU64,
    packet_interval_max_us: AtomicU64,
}

struct LocalSendPipeline {
    state: Arc<PipelineState>,
    handle: Option<JoinHandle<()>>,
}

struct PipelineState {
    queues: Mutex<WorkerQueues>,
    cv: Condvar,
    stop: AtomicBool,
    speaking: AtomicBool,
    last_error: Mutex<Option<String>>,
    stats: WorkerStats,
    /// When set, encoded packets are handed off directly to this handler from
    /// the encode thread instead of being queued for `_process()` drain.
    direct_send_handler: RwLock<Option<DirectSendHandler>>,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            queues: Mutex::new(WorkerQueues::default()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            speaking: AtomicBool::new(false),
            last_error: Mutex::new(None),
            stats: WorkerStats::default(),
            direct_send_handler: RwLock::new(None),
        }
    }

    fn record_error(&self, message: String) {
        let mut guard = self.last_error.lock().expect("last_error mutex poisoned");
        *guard = Some(message);
    }
}

#[derive(GodotClass)]
#[class(base=Node)]
pub struct NetworkAudioSender {
    base: Base<Node>,
    clock_origin: Instant,
    #[export]
    bitrate_bps: i32,
    #[export]
    input_sample_rate_hz: i32,
    #[export]
    enable_dtx: bool,
    #[export]
    denoise: bool,
    #[export]
    capture_audio_server_input: bool,
    #[export]
    microphone_frame_budget: i32,
    sender: Option<LocalSendPipeline>,
    loopback_target: Option<LoopbackTarget>,
    direct_send_handler: Option<DirectSendHandler>,
    last_speaking: bool,
    last_error: GString,
    captured_input_frames: i64,
    process_ticks: i64,
    empty_input_polls: i64,
    chunk_count: i64,
    input_samples_pushed: i64,
    max_available_frames: i32,
    max_chunk_frames: i32,
    last_available_frames: i32,
    last_chunk_frames: i32,
    last_capture_pull_us: u64,
    capture_pull_count: i64,
    capture_pull_interval_sum_us: u64,
    capture_pull_interval_max_us: u64,
    current_empty_input_poll_streak: i64,
    max_empty_input_poll_streak: i64,
    last_drain_nonempty_us: u64,
    drain_nonempty_count: i64,
    drain_nonempty_interval_sum_us: u64,
    drain_nonempty_interval_max_us: u64,
    max_drain_batch_packets: i64,
    last_drain_batch_packets: i64,
    packets_sent: i64,
    worker_pcm_dropped: i64,
    worker_packets_dropped: i64,
}

#[godot_api]
impl INode for NetworkAudioSender {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            clock_origin: Instant::now(),
            bitrate_bps: 16_000,
            input_sample_rate_hz: 48_000,
            enable_dtx: true,
            denoise: false,
            capture_audio_server_input: false,
            microphone_frame_budget: DEFAULT_MICROPHONE_FRAME_BUDGET,
            sender: None,
            loopback_target: None,
            direct_send_handler: None,
            last_speaking: false,
            last_error: GString::new(),
            captured_input_frames: 0,
            process_ticks: 0,
            empty_input_polls: 0,
            chunk_count: 0,
            input_samples_pushed: 0,
            max_available_frames: 0,
            max_chunk_frames: 0,
            last_available_frames: 0,
            last_chunk_frames: 0,
            last_capture_pull_us: 0,
            capture_pull_count: 0,
            capture_pull_interval_sum_us: 0,
            capture_pull_interval_max_us: 0,
            current_empty_input_poll_streak: 0,
            max_empty_input_poll_streak: 0,
            last_drain_nonempty_us: 0,
            drain_nonempty_count: 0,
            drain_nonempty_interval_sum_us: 0,
            drain_nonempty_interval_max_us: 0,
            max_drain_batch_packets: 0,
            last_drain_batch_packets: 0,
            packets_sent: 0,
            worker_pcm_dropped: 0,
            worker_packets_dropped: 0,
        }
    }

    fn ready(&mut self) {
        self.rebuild_encoder();
        // Always enable process so drain_paced_queue() runs regardless of capture mode.
        self.base_mut().set_process(true);
        if self.capture_audio_server_input {
            let mut audio_server = AudioServer::singleton();
            let _ = audio_server.set_input_device_active(true);
            self.input_sample_rate_hz = audio_server.get_input_mix_rate() as i32;
            self.rebuild_encoder();
        }
    }

    fn process(&mut self, _delta: f64) {
        self.process_ticks += 1;
        if self.capture_audio_server_input {
            self.pump_audio_server_input();
        }
        self.drain_worker_packets();
    }

    fn exit_tree(&mut self) {
        self.stop_sender();
        if self.capture_audio_server_input {
            let mut audio_server = AudioServer::singleton();
            let _ = audio_server.set_input_device_active(false);
        }
    }
}

#[godot_api]
impl NetworkAudioSender {
    #[signal]
    fn packet_ready(bytes: PackedByteArray);

    #[signal]
    fn encoder_error(message: GString);

    #[func]
    fn rebuild(&mut self) {
        self.rebuild_encoder();
    }

    #[func]
    fn push_pcm_mono(&mut self, samples: PackedFloat32Array) -> i32 {
        self.push_pcm_slice(&samples.to_vec())
    }

    #[func]
    fn connect_loopback_stream(&mut self, stream: Gd<AudioStreamNetwork>) {
        let target = stream.bind().loopback_target();
        self.loopback_target = Some(target);
        self.rebuild_encoder();
    }

    #[func]
    fn disconnect_loopback_stream(&mut self) {
        self.loopback_target = None;
        self.rebuild_encoder();
    }

    #[func]
    fn flush(&mut self) {
        if let Some(sender) = self.sender.as_ref() {
            sender.flush();
        }
        self.last_speaking = false;
    }

    #[func]
    fn start_capture(&mut self) {
        if self.capture_audio_server_input {
            return;
        }
        self.capture_audio_server_input = true;
        let mut audio_server = AudioServer::singleton();
        let _ = audio_server.set_input_device_active(true);
        self.input_sample_rate_hz = audio_server.get_input_mix_rate() as i32;
        self.rebuild_encoder();
    }

    #[func]
    fn stop_capture(&mut self) {
        if !self.capture_audio_server_input {
            return;
        }
        self.capture_audio_server_input = false;
        self.stop_sender();
        let mut audio_server = AudioServer::singleton();
        let _ = audio_server.set_input_device_active(false);
    }

    #[func]
    fn is_speaking(&self) -> bool {
        self.last_speaking
    }

    #[func]
    fn get_last_error(&self) -> GString {
        self.last_error.clone()
    }

    #[func]
    fn get_captured_input_frames(&self) -> i64 {
        self.captured_input_frames
    }

    #[func]
    fn get_stats(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set("captured_input_frames", self.captured_input_frames);
        dict.set("input_sample_rate_hz", self.input_sample_rate_hz);
        dict.set("bitrate_bps", self.bitrate_bps);
        dict.set("process_ticks", self.process_ticks);
        dict.set("empty_input_polls", self.empty_input_polls);
        dict.set("chunk_count", self.chunk_count);
        dict.set("packets_sent", self.packets_sent);
        dict.set("input_samples_pushed", self.input_samples_pushed);
        dict.set("max_available_frames", self.max_available_frames);
        dict.set("max_chunk_frames", self.max_chunk_frames);
        dict.set("last_available_frames", self.last_available_frames);
        dict.set("last_chunk_frames", self.last_chunk_frames);
        dict.set(
            "avg_capture_pull_interval_ms",
            if self.capture_pull_count > 0 {
                self.capture_pull_interval_sum_us as f64 / self.capture_pull_count as f64 / 1000.0
            } else {
                0.0
            },
        );
        dict.set(
            "max_capture_pull_interval_ms",
            self.capture_pull_interval_max_us as f64 / 1000.0,
        );
        dict.set(
            "current_empty_input_poll_streak",
            self.current_empty_input_poll_streak,
        );
        dict.set(
            "max_empty_input_poll_streak",
            self.max_empty_input_poll_streak,
        );
        dict.set(
            "avg_main_drain_interval_ms",
            if self.drain_nonempty_count > 0 {
                self.drain_nonempty_interval_sum_us as f64
                    / self.drain_nonempty_count as f64
                    / 1000.0
            } else {
                0.0
            },
        );
        dict.set(
            "max_main_drain_interval_ms",
            self.drain_nonempty_interval_max_us as f64 / 1000.0,
        );
        dict.set("max_drain_batch_packets", self.max_drain_batch_packets);
        dict.set("last_drain_batch_packets", self.last_drain_batch_packets);
        dict.set(
            "capture_audio_server_input",
            self.capture_audio_server_input,
        );
        if let Some(sender) = self.sender.as_ref() {
            let stats = &sender.state.stats;
            let interval_count = stats.packet_interval_count.load(Ordering::Relaxed);
            let tick_lag_count = stats.tick_lag_count.load(Ordering::Relaxed);
            dict.set(
                "emitted_packets",
                stats.packets_emitted.load(Ordering::Relaxed),
            );
            dict.set("worker_ticks", stats.worker_ticks.load(Ordering::Relaxed));
            dict.set(
                "worker_ticks_with_pcm",
                stats.worker_ticks_with_pcm.load(Ordering::Relaxed),
            );
            dict.set(
                "worker_empty_pcm_ticks",
                stats.worker_empty_pcm_ticks.load(Ordering::Relaxed),
            );
            dict.set(
                "worker_partial_pcm_ticks",
                stats.worker_partial_pcm_ticks.load(Ordering::Relaxed),
            );
            dict.set(
                "worker_ticks_with_packets",
                stats.worker_ticks_with_packets.load(Ordering::Relaxed),
            );
            dict.set(
                "worker_ticks_without_packets",
                stats.worker_ticks_without_packets.load(Ordering::Relaxed),
            );
            dict.set("silent_ticks", stats.silent_ticks.load(Ordering::Relaxed));
            dict.set(
                "queued_packets",
                stats.queued_packets.load(Ordering::Relaxed),
            );
            dict.set(
                "queued_pcm_samples",
                stats.queued_pcm_samples.load(Ordering::Relaxed),
            );
            dict.set(
                "worker_pcm_samples_dropped",
                stats.pcm_samples_dropped.load(Ordering::Relaxed),
            );
            dict.set(
                "worker_packets_dropped",
                stats.packets_dropped.load(Ordering::Relaxed),
            );
            dict.set(
                "avg_tick_lag_ms",
                if tick_lag_count > 0 {
                    stats.tick_lag_sum_us.load(Ordering::Relaxed) as f64
                        / tick_lag_count as f64
                        / 1000.0
                } else {
                    0.0
                },
            );
            dict.set(
                "max_tick_lag_ms",
                stats.tick_lag_max_us.load(Ordering::Relaxed) as f64 / 1000.0,
            );
            dict.set(
                "avg_packet_interval_ms",
                if interval_count > 0 {
                    stats.packet_interval_sum_us.load(Ordering::Relaxed) as f64
                        / interval_count as f64
                        / 1000.0
                } else {
                    0.0
                },
            );
            dict.set(
                "max_packet_interval_ms",
                stats.packet_interval_max_us.load(Ordering::Relaxed) as f64 / 1000.0,
            );
        } else {
            dict.set("emitted_packets", 0_i64);
            dict.set("worker_ticks", 0_i64);
            dict.set("worker_ticks_with_pcm", 0_i64);
            dict.set("worker_empty_pcm_ticks", 0_i64);
            dict.set("worker_partial_pcm_ticks", 0_i64);
            dict.set("worker_ticks_with_packets", 0_i64);
            dict.set("worker_ticks_without_packets", 0_i64);
            dict.set("silent_ticks", 0_i64);
            dict.set("queued_packets", 0_i64);
            dict.set("queued_pcm_samples", 0_i64);
            dict.set("worker_pcm_samples_dropped", self.worker_pcm_dropped);
            dict.set("worker_packets_dropped", self.worker_packets_dropped);
            dict.set("avg_tick_lag_ms", 0.0_f64);
            dict.set("max_tick_lag_ms", 0.0_f64);
            dict.set("avg_packet_interval_ms", 0.0_f64);
            dict.set("max_packet_interval_ms", 0.0_f64);
        }
        dict
    }
}

impl NetworkAudioSender {
    fn rebuild_encoder(&mut self) {
        self.stop_sender();
        let config = VoiceEncoderConfig {
            input_sample_rate: self.input_sample_rate_hz.max(1) as u32,
            frame_duration_ms: 20,
            bitrate_bps: self.bitrate_bps,
            enable_dtx: self.enable_dtx,
            denoise: self.denoise,
        };

        match LocalSendPipeline::new(config, self.loopback_target.clone()) {
            Ok(sender) => {
                // Reinstall the direct send handler on the new pipeline if one was registered.
                if let Some(handler) = self.direct_send_handler.clone() {
                    sender.set_direct_send_handler(handler);
                }
                self.sender = Some(sender);
                self.last_error = GString::new();
            }
            Err(err) => {
                let message = GString::from(err.to_string().as_str());
                self.sender = None;
                self.last_error = message.clone();
                self.base_mut()
                    .emit_signal("encoder_error", &[message.to_variant()]);
            }
        }
    }

    /// Install a handler that receives encoded packet bytes directly from the
    /// encode thread, bypassing `_process()`. When set, the `packet_ready`
    /// signal is NOT emitted (the handler replaces it).
    pub(crate) fn install_direct_send_handler(&mut self, handler: DirectSendHandler) {
        self.direct_send_handler = Some(handler.clone());
        if let Some(sender) = self.sender.as_ref() {
            sender.set_direct_send_handler(handler);
        }
    }

    fn pump_audio_server_input(&mut self) {
        let audio_server = AudioServer::singleton();
        let mut available = audio_server.get_input_frames_available();
        if available <= 0 {
            self.empty_input_polls += 1;
            self.current_empty_input_poll_streak += 1;
            self.max_empty_input_poll_streak = self
                .max_empty_input_poll_streak
                .max(self.current_empty_input_poll_streak);
            return;
        }

        self.current_empty_input_poll_streak = 0;
        self.last_available_frames = available;
        self.max_available_frames = self.max_available_frames.max(available);
        let now_us = self.clock_origin.elapsed().as_micros() as u64;
        if self.last_capture_pull_us != 0 {
            let interval_us = now_us.saturating_sub(self.last_capture_pull_us);
            self.capture_pull_count += 1;
            self.capture_pull_interval_sum_us += interval_us;
            self.capture_pull_interval_max_us = self.capture_pull_interval_max_us.max(interval_us);
        }
        self.last_capture_pull_us = now_us;
        let frame_budget = self.microphone_frame_budget.max(1);
        while available > 0 {
            let chunk_frames = available.min(frame_budget);
            let stereo_frames = audio_server.get_input_frames(chunk_frames);
            if stereo_frames.is_empty() {
                break;
            }

            let stereo = stereo_frames.to_vec();
            let mut mono_samples = Vec::with_capacity(stereo.len());
            for frame in &stereo {
                mono_samples.push(0.5 * (frame.x + frame.y));
            }

            self.chunk_count += 1;
            self.last_chunk_frames = stereo.len() as i32;
            self.max_chunk_frames = self.max_chunk_frames.max(stereo.len() as i32);
            self.captured_input_frames += stereo.len() as i64;
            let _ = self.push_pcm_slice(&mono_samples);
            available -= stereo.len() as i32;
        }
    }

    fn push_pcm_slice(&mut self, samples: &[f32]) -> i32 {
        if self.sender.is_none() {
            self.rebuild_encoder();
        }

        let Some(sender) = self.sender.as_ref() else {
            return 0;
        };

        self.input_samples_pushed += samples.len() as i64;
        sender.push_input_pcm(samples);
        sender.queued_packet_count()
    }

    fn drain_worker_packets(&mut self) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };

        let mut packets = Vec::new();
        while let Some(packet_bytes) = sender.pop_encoded_packet() {
            packets.push(packet_bytes);
        }
        self.last_drain_batch_packets = packets.len() as i64;
        self.max_drain_batch_packets = self
            .max_drain_batch_packets
            .max(self.last_drain_batch_packets);
        if !packets.is_empty() {
            let now_us = self.clock_origin.elapsed().as_micros() as u64;
            if self.last_drain_nonempty_us != 0 {
                let interval_us = now_us.saturating_sub(self.last_drain_nonempty_us);
                self.drain_nonempty_count += 1;
                self.drain_nonempty_interval_sum_us += interval_us;
                self.drain_nonempty_interval_max_us =
                    self.drain_nonempty_interval_max_us.max(interval_us);
            }
            self.last_drain_nonempty_us = now_us;
        }
        let speaking = sender.is_speaking();
        let last_error = sender.take_last_error();
        let worker_pcm_dropped = sender
            .state
            .stats
            .pcm_samples_dropped
            .load(Ordering::Relaxed);
        let worker_packets_dropped = sender.state.stats.packets_dropped.load(Ordering::Relaxed);

        for packet_bytes in packets {
            self.packets_sent += 1;
            self.last_speaking = speaking;
            let packed: PackedByteArray = packet_bytes.into_iter().collect();
            self.base_mut()
                .emit_signal("packet_ready", &[packed.to_variant()]);
        }

        if let Some(message) = last_error {
            self.last_error = GString::from(message.as_str());
            let message_variant = self.last_error.to_variant();
            self.base_mut()
                .emit_signal("encoder_error", &[message_variant]);
        }

        self.worker_pcm_dropped = worker_pcm_dropped;
        self.worker_packets_dropped = worker_packets_dropped;
    }

    fn stop_sender(&mut self) {
        if let Some(mut sender) = self.sender.take() {
            sender.stop();
        }
    }
}

impl LocalSendPipeline {
    fn new(
        config: VoiceEncoderConfig,
        loopback_target: Option<LoopbackTarget>,
    ) -> anyhow::Result<Self> {
        let state = Arc::new(PipelineState::new());
        let thread_state = Arc::clone(&state);

        let handle = thread::Builder::new()
            .name("gna-send-pacer".to_string())
            .spawn(move || worker_loop(thread_state, config, loopback_target))?;

        Ok(Self {
            state,
            handle: Some(handle),
        })
    }

    fn push_input_pcm(&self, samples: &[f32]) {
        let mut guard = self
            .state
            .queues
            .lock()
            .expect("worker queue mutex poisoned");
        for &sample in samples {
            if guard.pcm_samples.len() >= MAX_PENDING_PCM_SAMPLES
                && guard.pcm_samples.pop_front().is_some()
            {
                self.state
                    .stats
                    .pcm_samples_dropped
                    .fetch_add(1, Ordering::Relaxed);
            }
            guard.pcm_samples.push_back(sample);
        }
        self.state
            .stats
            .queued_pcm_samples
            .store(guard.pcm_samples.len() as i64, Ordering::Relaxed);
        drop(guard);
        self.state.cv.notify_one();
    }

    fn pop_encoded_packet(&self) -> Option<Vec<u8>> {
        let mut guard = self
            .state
            .queues
            .lock()
            .expect("worker queue mutex poisoned");
        let packet = guard.encoded_packets.pop_front();
        self.state
            .stats
            .queued_packets
            .store(guard.encoded_packets.len() as i64, Ordering::Relaxed);
        packet
    }

    fn queued_packet_count(&self) -> i32 {
        let guard = self
            .state
            .queues
            .lock()
            .expect("worker queue mutex poisoned");
        guard.encoded_packets.len() as i32
    }

    fn take_last_error(&self) -> Option<String> {
        let mut guard = self
            .state
            .last_error
            .lock()
            .expect("last_error mutex poisoned");
        guard.take()
    }

    fn is_speaking(&self) -> bool {
        self.state.speaking.load(Ordering::Relaxed)
    }

    fn flush(&self) {
        let mut guard = self
            .state
            .queues
            .lock()
            .expect("worker queue mutex poisoned");
        guard.pcm_samples.clear();
        guard.encoded_packets.clear();
        self.state
            .stats
            .queued_pcm_samples
            .store(0, Ordering::Relaxed);
        self.state.stats.queued_packets.store(0, Ordering::Relaxed);
    }

    fn set_direct_send_handler(&self, handler: DirectSendHandler) {
        *self
            .state
            .direct_send_handler
            .write()
            .expect("direct_send_handler poisoned") = Some(handler);
    }

    fn stop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        self.state.cv.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LocalSendPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(
    state: Arc<PipelineState>,
    config: VoiceEncoderConfig,
    loopback_target: Option<LoopbackTarget>,
) {
    let mut encoder = match VoiceEncoder::new(config) {
        Ok(encoder) => encoder,
        Err(err) => {
            state.record_error(err.to_string());
            return;
        }
    };

    let epoch = Instant::now();
    let mut next_tick = epoch;
    loop {
        if state.stop.load(Ordering::Relaxed) {
            break;
        }

        let now = Instant::now();
        if now < next_tick {
            let timeout = next_tick.saturating_duration_since(now);
            let guard = state.queues.lock().expect("worker queue mutex poisoned");
            let _ = state
                .cv
                .wait_timeout(guard, timeout)
                .expect("worker condvar poisoned");
            continue;
        }

        state.stats.worker_ticks.fetch_add(1, Ordering::Relaxed);
        let tick_lag_us = now.saturating_duration_since(next_tick).as_micros() as u64;
        state.stats.tick_lag_count.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .tick_lag_sum_us
            .fetch_add(tick_lag_us, Ordering::Relaxed);
        state
            .stats
            .tick_lag_max_us
            .fetch_max(tick_lag_us, Ordering::Relaxed);

        let frame = {
            let mut guard = state.queues.lock().expect("worker queue mutex poisoned");
            let take = guard.pcm_samples.len().min(960);
            let mut frame = Vec::with_capacity(take);
            for _ in 0..take {
                if let Some(sample) = guard.pcm_samples.pop_front() {
                    frame.push(sample);
                }
            }
            state
                .stats
                .queued_pcm_samples
                .store(guard.pcm_samples.len() as i64, Ordering::Relaxed);
            frame
        };

        let mut emitted_this_tick = 0_i64;
        if !frame.is_empty() {
            state
                .stats
                .worker_ticks_with_pcm
                .fetch_add(1, Ordering::Relaxed);
            if frame.len() < 960 {
                state
                    .stats
                    .worker_partial_pcm_ticks
                    .fetch_add(1, Ordering::Relaxed);
            }
            encoder.push_pcm(&frame);
            loop {
                match encoder.poll_packet() {
                    Ok(Some(packet)) => {
                        state.speaking.store(
                            !packet.flags.contains(PacketFlags::END_OF_TALKSPURT),
                            Ordering::Relaxed,
                        );
                        if let Some(target) = loopback_target.as_ref() {
                            let _ = target.enqueue_now(packet.clone());
                        }
                        let bytes = encode_packet_bytes(&packet).to_vec();
                        // If a direct send handler is installed, call it immediately
                        // from this thread instead of queuing for _process() drain.
                        let handler = state
                            .direct_send_handler
                            .read()
                            .expect("direct_send_handler poisoned")
                            .clone();
                        if let Some(ref h) = handler {
                            h(bytes);
                        } else {
                            let mut guard =
                                state.queues.lock().expect("worker queue mutex poisoned");
                            if guard.encoded_packets.len() >= MAX_PENDING_PACKETS {
                                let _ = guard.encoded_packets.pop_front();
                                state.stats.packets_dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            guard.encoded_packets.push_back(bytes);
                            state
                                .stats
                                .queued_packets
                                .store(guard.encoded_packets.len() as i64, Ordering::Relaxed);
                            drop(guard);
                        }

                        let emit_us = epoch.elapsed().as_micros() as u64;
                        let prev = state
                            .stats
                            .last_packet_emit_us
                            .swap(emit_us, Ordering::Relaxed);
                        if prev != 0 {
                            let delta = emit_us.saturating_sub(prev);
                            state
                                .stats
                                .packet_interval_count
                                .fetch_add(1, Ordering::Relaxed);
                            state
                                .stats
                                .packet_interval_sum_us
                                .fetch_add(delta, Ordering::Relaxed);
                            state
                                .stats
                                .packet_interval_max_us
                                .fetch_max(delta, Ordering::Relaxed);
                        }
                        state.stats.packets_emitted.fetch_add(1, Ordering::Relaxed);
                        emitted_this_tick += 1;
                    }
                    Ok(None) => break,
                    Err(err) => {
                        state.record_error(err.to_string());
                        break;
                    }
                }
            }
        } else {
            state
                .stats
                .worker_empty_pcm_ticks
                .fetch_add(1, Ordering::Relaxed);
        }

        if emitted_this_tick > 0 {
            state
                .stats
                .worker_ticks_with_packets
                .fetch_add(1, Ordering::Relaxed);
        } else {
            state
                .stats
                .worker_ticks_without_packets
                .fetch_add(1, Ordering::Relaxed);
            if !frame.is_empty() {
                state.stats.silent_ticks.fetch_add(1, Ordering::Relaxed);
            }
        }

        next_tick += std::time::Duration::from_micros(PACE_INTERVAL_US);
        if next_tick + std::time::Duration::from_secs(10) < Instant::now() {
            next_tick = Instant::now();
        }
    }
}

use godot::builtin::{PackedByteArray, PackedFloat32Array, VarDictionary};
use godot::classes::{AudioServer, INode, Node};
use godot::obj::Singleton;
use godot::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;
use voice_core::vad::VadConfig;
use voice_core::{PacketFlags, VoiceEncoder, VoiceEncoderConfig};

use crate::packet_bytes::encode_packet_bytes;

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
    packets_emitted: AtomicI64,
    packets_dropped: AtomicI64,
    pcm_samples_dropped: AtomicI64,
    queued_pcm_samples: AtomicI64,
    queued_packets: AtomicI64,
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
    force_transmit: AtomicBool,
    last_error: Mutex<Option<String>>,
    stats: WorkerStats,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            queues: Mutex::new(WorkerQueues::default()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            speaking: AtomicBool::new(false),
            force_transmit: AtomicBool::new(false),
            last_error: Mutex::new(None),
            stats: WorkerStats::default(),
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
    #[export]
    bitrate_bps: i32,
    #[export]
    input_sample_rate_hz: i32,
    #[export]
    vad_threshold_db: f32,
    #[export]
    push_to_talk: bool,
    #[export]
    denoise: bool,
    #[export]
    capture_audio_server_input: bool,
    #[export]
    microphone_frame_budget: i32,
    sender: Option<LocalSendPipeline>,
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
    packets_sent: i64,
    worker_pcm_dropped: i64,
    worker_packets_dropped: i64,
}

#[godot_api]
impl INode for NetworkAudioSender {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            bitrate_bps: 16_000,
            input_sample_rate_hz: 48_000,
            vad_threshold_db: -45.0,
            push_to_talk: false,
            denoise: false,
            capture_audio_server_input: false,
            microphone_frame_budget: DEFAULT_MICROPHONE_FRAME_BUDGET,
            sender: None,
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
            "capture_audio_server_input",
            self.capture_audio_server_input,
        );
        if let Some(sender) = self.sender.as_ref() {
            let stats = &sender.state.stats;
            let interval_count = stats.packet_interval_count.load(Ordering::Relaxed);
            dict.set(
                "emitted_packets",
                stats.packets_emitted.load(Ordering::Relaxed),
            );
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
            dict.set("queued_packets", 0_i64);
            dict.set("queued_pcm_samples", 0_i64);
            dict.set("worker_pcm_samples_dropped", self.worker_pcm_dropped);
            dict.set("worker_packets_dropped", self.worker_packets_dropped);
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
            vad: VadConfig {
                threshold_db: self.vad_threshold_db,
                hangover_frames: VadConfig::default().hangover_frames,
            },
            denoise: self.denoise,
        };

        match LocalSendPipeline::new(config, self.push_to_talk) {
            Ok(sender) => {
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

    fn pump_audio_server_input(&mut self) {
        let audio_server = AudioServer::singleton();
        let mut available = audio_server.get_input_frames_available();
        if available <= 0 {
            self.empty_input_polls += 1;
            return;
        }

        self.last_available_frames = available;
        self.max_available_frames = self.max_available_frames.max(available);
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
        sender.push_input_pcm(samples, self.push_to_talk);
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
    fn new(config: VoiceEncoderConfig, force_transmit: bool) -> anyhow::Result<Self> {
        let state = Arc::new(PipelineState::new());
        state
            .force_transmit
            .store(force_transmit, Ordering::Relaxed);
        let thread_state = Arc::clone(&state);

        let handle = thread::Builder::new()
            .name("gna-send-pacer".to_string())
            .spawn(move || worker_loop(thread_state, config))?;

        Ok(Self {
            state,
            handle: Some(handle),
        })
    }

    fn push_input_pcm(&self, samples: &[f32], force_transmit: bool) {
        self.state
            .force_transmit
            .store(force_transmit, Ordering::Relaxed);
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

fn worker_loop(state: Arc<PipelineState>, config: VoiceEncoderConfig) {
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

        let force_transmit = state.force_transmit.load(Ordering::Relaxed);
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

        if !frame.is_empty() {
            encoder.set_force_transmit(force_transmit);
            encoder.push_pcm(&frame);
            loop {
                match encoder.poll_packet() {
                    Ok(Some(packet)) => {
                        state.speaking.store(
                            !packet.flags.contains(PacketFlags::END_OF_TALKSPURT),
                            Ordering::Relaxed,
                        );
                        let bytes = encode_packet_bytes(&packet).to_vec();
                        let mut guard = state.queues.lock().expect("worker queue mutex poisoned");
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
                    }
                    Ok(None) => break,
                    Err(err) => {
                        state.record_error(err.to_string());
                        break;
                    }
                }
            }
        }

        next_tick += std::time::Duration::from_micros(PACE_INTERVAL_US);
        if next_tick + std::time::Duration::from_secs(10) < Instant::now() {
            next_tick = Instant::now();
        }
    }
}

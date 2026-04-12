use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_queue::ArrayQueue;
use godot::builtin::{GString, PackedByteArray, VarDictionary};
use godot::classes::native::AudioFrame;
use godot::classes::{
    AudioStream, AudioStreamPlayback, AudioStreamPlaybackResampled, IAudioStream,
    IAudioStreamPlaybackResampled,
};
use godot::meta::conv::RawPtr;
use godot::obj::Gd;
use godot::prelude::*;
use voice_core::{PacketArrival, ReceiverStats, VoicePacket, VoiceReceiver};

use crate::packet_bytes::decode_packet_bytes;

const DEFAULT_QUEUE_CAPACITY: usize = 256;
const PLAYBACK_DRAIN_BUDGET: usize = 32;
const RECEIVER_SAMPLE_RATE_HZ: u32 = 48_000;
const RECEIVER_MIN_DELAY_MS: u32 = 20;
const RECEIVER_FRAME_SAMPLES: usize = 480;

#[derive(Debug)]
struct QueuedPacket {
    packet: VoicePacket,
    arrival: PacketArrival,
}

#[derive(Debug)]
struct SharedStreamState {
    queue: ArrayQueue<QueuedPacket>,
    mono_epoch: Instant,
    dropped_packets: AtomicU64,
    current_buffer_size_ms: AtomicU32,
    target_delay_ms: AtomicU32,
    preferred_buffer_size_ms: AtomicU32,
    packets_awaiting_decode: AtomicU32,
    packets_per_sec: AtomicU32,
    expand_rate: AtomicU32,
    accelerate_rate: AtomicU32,
    preemptive_rate: AtomicU32,
    expand_per_sec_milli: AtomicU32,
    accelerate_per_sec_milli: AtomicU32,
    preemptive_expand_per_sec_milli: AtomicU32,
    normal_per_sec_milli: AtomicU32,
    concealed_samples: AtomicU64,
    consecutive_failures: AtomicU32,
    intentional_silence: AtomicBool,
    playing: AtomicBool,
}

impl SharedStreamState {
    fn new() -> Self {
        Self {
            queue: ArrayQueue::new(DEFAULT_QUEUE_CAPACITY),
            mono_epoch: Instant::now(),
            dropped_packets: AtomicU64::new(0),
            current_buffer_size_ms: AtomicU32::new(0),
            target_delay_ms: AtomicU32::new(0),
            preferred_buffer_size_ms: AtomicU32::new(0),
            packets_awaiting_decode: AtomicU32::new(0),
            packets_per_sec: AtomicU32::new(0),
            expand_rate: AtomicU32::new(0),
            accelerate_rate: AtomicU32::new(0),
            preemptive_rate: AtomicU32::new(0),
            expand_per_sec_milli: AtomicU32::new(0),
            accelerate_per_sec_milli: AtomicU32::new(0),
            preemptive_expand_per_sec_milli: AtomicU32::new(0),
            normal_per_sec_milli: AtomicU32::new(0),
            concealed_samples: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            intentional_silence: AtomicBool::new(false),
            playing: AtomicBool::new(false),
        }
    }

    fn now_mono_us(&self) -> u64 {
        self.mono_epoch.elapsed().as_micros() as u64
    }

    fn reset_runtime_stats(&self) {
        self.current_buffer_size_ms.store(0, Ordering::Relaxed);
        self.target_delay_ms.store(0, Ordering::Relaxed);
        self.preferred_buffer_size_ms.store(0, Ordering::Relaxed);
        self.packets_awaiting_decode.store(0, Ordering::Relaxed);
        self.packets_per_sec.store(0, Ordering::Relaxed);
        self.expand_rate.store(0, Ordering::Relaxed);
        self.accelerate_rate.store(0, Ordering::Relaxed);
        self.preemptive_rate.store(0, Ordering::Relaxed);
        self.expand_per_sec_milli.store(0, Ordering::Relaxed);
        self.accelerate_per_sec_milli.store(0, Ordering::Relaxed);
        self.preemptive_expand_per_sec_milli
            .store(0, Ordering::Relaxed);
        self.normal_per_sec_milli.store(0, Ordering::Relaxed);
        self.concealed_samples.store(0, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.intentional_silence.store(false, Ordering::Relaxed);
    }

    fn publish_stats(&self, stats: &ReceiverStats) {
        self.current_buffer_size_ms
            .store(stats.current_buffer_size_ms, Ordering::Relaxed);
        self.target_delay_ms
            .store(stats.target_delay_ms, Ordering::Relaxed);
        self.preferred_buffer_size_ms
            .store(stats.preferred_buffer_size_ms, Ordering::Relaxed);
        self.packets_awaiting_decode
            .store(stats.packets_awaiting_decode as u32, Ordering::Relaxed);
        self.packets_per_sec
            .store(stats.packets_per_sec, Ordering::Relaxed);
        self.expand_rate
            .store(stats.expand_rate as u32, Ordering::Relaxed);
        self.accelerate_rate
            .store(stats.accelerate_rate as u32, Ordering::Relaxed);
        self.preemptive_rate
            .store(stats.preemptive_rate as u32, Ordering::Relaxed);
        self.expand_per_sec_milli.store(
            (stats.expand_per_sec * 1000.0).round().max(0.0) as u32,
            Ordering::Relaxed,
        );
        self.accelerate_per_sec_milli.store(
            (stats.accelerate_per_sec * 1000.0).round().max(0.0) as u32,
            Ordering::Relaxed,
        );
        self.preemptive_expand_per_sec_milli.store(
            (stats.preemptive_expand_per_sec * 1000.0).round().max(0.0) as u32,
            Ordering::Relaxed,
        );
        self.normal_per_sec_milli.store(
            (stats.normal_per_sec * 1000.0).round().max(0.0) as u32,
            Ordering::Relaxed,
        );
        self.concealed_samples
            .store(stats.concealed_samples, Ordering::Relaxed);
        self.consecutive_failures
            .store(stats.consecutive_failures, Ordering::Relaxed);
        self.intentional_silence
            .store(stats.intentional_silence, Ordering::Relaxed);
    }
}

#[derive(GodotClass)]
#[class(base=AudioStream)]
pub struct AudioStreamNetwork {
    base: Base<AudioStream>,
    #[export]
    max_delay_ms: i32,
    shared: Arc<SharedStreamState>,
}

#[derive(GodotClass)]
#[class(base=AudioStreamPlaybackResampled)]
struct AudioStreamNetworkPlayback {
    base: Base<AudioStreamPlaybackResampled>,
    shared: Arc<SharedStreamState>,
    receiver: Option<VoiceReceiver>,
    pending_mono: Vec<f32>,
    pending_cursor: usize,
    playback_position_frames: u64,
    detached_init: bool,
    warned_detached_init: bool,
}

#[godot_api]
impl IAudioStream for AudioStreamNetwork {
    fn init(base: Base<AudioStream>) -> Self {
        Self {
            base,
            max_delay_ms: 120,
            shared: Arc::new(SharedStreamState::new()),
        }
    }

    fn instantiate_playback(&self) -> Option<Gd<AudioStreamPlayback>> {
        let shared = Arc::clone(&self.shared);
        let max_delay_ms = self.max_delay_ms.max(RECEIVER_MIN_DELAY_MS as i32) as u32;
        let playback = Gd::from_init_fn(move |base| AudioStreamNetworkPlayback {
            base,
            shared,
            receiver: build_receiver(max_delay_ms),
            pending_mono: Vec::new(),
            pending_cursor: 0,
            playback_position_frames: 0,
            detached_init: false,
            warned_detached_init: false,
        });
        Some(playback.upcast())
    }

    fn get_stream_name(&self) -> GString {
        "NetworkAudio".into()
    }

    fn get_length(&self) -> f64 {
        0.0
    }

    fn is_monophonic(&self) -> bool {
        true
    }
}

#[godot_api]
impl AudioStreamNetwork {
    #[func]
    fn push_packet(&mut self, bytes: PackedByteArray) -> bool {
        self.push_packet_with_meta(bytes, self.shared.now_mono_us() as i64)
    }

    #[func]
    fn push_packet_with_meta(&mut self, bytes: PackedByteArray, received_at_mono_us: i64) -> bool {
        let Ok(packet) = decode_packet_bytes(&bytes) else {
            return false;
        };

        self.enqueue_packet(
            packet,
            PacketArrival {
                received_at_mono_us: received_at_mono_us.max(0) as u64,
            },
        )
    }

    #[func]
    fn queued_packet_count(&self) -> i32 {
        self.shared.queue.len() as i32
    }

    #[func]
    fn clear_pending_packets(&mut self) {
        while self.shared.queue.pop().is_some() {}
    }

    #[func]
    fn get_buffer_ms(&self) -> i32 {
        self.shared.current_buffer_size_ms.load(Ordering::Relaxed) as i32
    }

    #[func]
    fn get_stats(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set(
            "current_buffer_size_ms",
            self.shared.current_buffer_size_ms.load(Ordering::Relaxed),
        );
        dict.set(
            "target_delay_ms",
            self.shared.target_delay_ms.load(Ordering::Relaxed),
        );
        dict.set(
            "preferred_buffer_size_ms",
            self.shared.preferred_buffer_size_ms.load(Ordering::Relaxed),
        );
        dict.set(
            "packets_awaiting_decode",
            self.shared.packets_awaiting_decode.load(Ordering::Relaxed) as i64,
        );
        dict.set(
            "packets_per_sec",
            self.shared.packets_per_sec.load(Ordering::Relaxed),
        );
        dict.set(
            "expand_rate",
            self.shared.expand_rate.load(Ordering::Relaxed),
        );
        dict.set(
            "accelerate_rate",
            self.shared.accelerate_rate.load(Ordering::Relaxed),
        );
        dict.set(
            "preemptive_rate",
            self.shared.preemptive_rate.load(Ordering::Relaxed),
        );
        dict.set(
            "expand_per_sec",
            self.shared.expand_per_sec_milli.load(Ordering::Relaxed) as f64 / 1000.0,
        );
        dict.set(
            "accelerate_per_sec",
            self.shared.accelerate_per_sec_milli.load(Ordering::Relaxed) as f64 / 1000.0,
        );
        dict.set(
            "preemptive_expand_per_sec",
            self.shared
                .preemptive_expand_per_sec_milli
                .load(Ordering::Relaxed) as f64
                / 1000.0,
        );
        dict.set(
            "normal_per_sec",
            self.shared.normal_per_sec_milli.load(Ordering::Relaxed) as f64 / 1000.0,
        );
        dict.set(
            "concealed_samples",
            self.shared.concealed_samples.load(Ordering::Relaxed) as i64,
        );
        dict.set(
            "consecutive_failures",
            self.shared.consecutive_failures.load(Ordering::Relaxed),
        );
        dict.set(
            "intentional_silence",
            self.shared.intentional_silence.load(Ordering::Relaxed),
        );
        dict.set("queued_packets", self.shared.queue.len() as i64);
        dict.set(
            "dropped_packets",
            self.shared.dropped_packets.load(Ordering::Relaxed) as i64,
        );
        dict.set("configured_max_delay_ms", self.max_delay_ms);
        dict.set("is_playing", self.shared.playing.load(Ordering::Relaxed));
        dict
    }
}

impl AudioStreamNetwork {
    fn enqueue_packet(&mut self, packet: VoicePacket, arrival: PacketArrival) -> bool {
        let queued = QueuedPacket { packet, arrival };
        match self.shared.queue.push(queued) {
            Ok(()) => true,
            Err(queued) => {
                let _ = self.shared.queue.pop();
                self.shared.dropped_packets.fetch_add(1, Ordering::Relaxed);
                self.shared.queue.push(queued).is_ok()
            }
        }
    }
}

#[godot_api]
impl IAudioStreamPlaybackResampled for AudioStreamNetworkPlayback {
    fn init(base: Base<AudioStreamPlaybackResampled>) -> Self {
        Self {
            base,
            shared: Arc::new(SharedStreamState::new()),
            receiver: build_receiver(120),
            pending_mono: Vec::new(),
            pending_cursor: 0,
            playback_position_frames: 0,
            detached_init: true,
            warned_detached_init: false,
        }
    }

    fn start(&mut self, _from_pos: f64) {
        self.playback_position_frames = 0;
        self.pending_mono.clear();
        self.pending_cursor = 0;
        self.shared.reset_runtime_stats();
        self.shared.playing.store(true, Ordering::Relaxed);
        self.base_mut().begin_resample();
    }

    fn stop(&mut self) {
        self.pending_mono.clear();
        self.pending_cursor = 0;
        self.shared.reset_runtime_stats();
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    fn get_playback_position(&self) -> f64 {
        self.playback_position_frames as f64 / RECEIVER_SAMPLE_RATE_HZ as f64
    }

    unsafe fn mix_resampled_rawptr(&mut self, buffer: RawPtr<*mut AudioFrame>, frames: i32) -> i32 {
        let buffer = buffer.ptr();
        if buffer.is_null() || frames <= 0 {
            return 0;
        }

        let frame_count = frames as usize;
        let out = unsafe { slice::from_raw_parts_mut(buffer, frame_count) };
        zero_frames(out);

        if !self.is_playing() {
            return frames;
        }

        if self.detached_init {
            if !self.warned_detached_init {
                godot_warn!(
                    "AudioStreamNetworkPlayback was initialized without shared stream state; output will stay silent"
                );
                self.warned_detached_init = true;
            }
            return frames;
        }

        self.drain_packets_into_receiver();
        self.fill_output_frames(out);

        if let Some(receiver) = self.receiver.as_ref() {
            let stats = receiver.stats();
            self.shared.publish_stats(&stats);
        }

        self.shared
            .packets_awaiting_decode
            .store(self.shared.queue.len() as u32, Ordering::Relaxed);
        self.playback_position_frames = self
            .playback_position_frames
            .saturating_add(frame_count as u64);

        frames
    }

    fn get_stream_sampling_rate(&self) -> f32 {
        RECEIVER_SAMPLE_RATE_HZ as f32
    }
}

impl AudioStreamNetworkPlayback {
    fn drain_packets_into_receiver(&mut self) {
        let Some(receiver) = self.receiver.as_mut() else {
            return;
        };

        let now_mono_us = self.shared.now_mono_us();
        for _ in 0..PLAYBACK_DRAIN_BUDGET {
            let Some(queued) = self.shared.queue.pop() else {
                break;
            };

            let _ = receiver.push_packet_with_now_mono(queued.packet, queued.arrival, now_mono_us);
        }
    }

    fn fill_output_frames(&mut self, out: &mut [AudioFrame]) {
        let Some(receiver) = self.receiver.as_mut() else {
            return;
        };

        let mut written = 0;
        while written < out.len() {
            if self.pending_cursor >= self.pending_mono.len() {
                self.pending_mono.resize(RECEIVER_FRAME_SAMPLES, 0.0);
                receiver.pull_frame(&mut self.pending_mono);
                self.pending_cursor = 0;
            }

            let available = self.pending_mono.len().saturating_sub(self.pending_cursor);
            if available == 0 {
                break;
            }

            let copy_count = available.min(out.len() - written);
            for frame in &mut out[written..written + copy_count] {
                let sample = self.pending_mono[self.pending_cursor];
                frame.left = sample;
                frame.right = sample;
                self.pending_cursor += 1;
            }
            written += copy_count;
        }
    }
}

fn build_receiver(max_delay_ms: u32) -> Option<VoiceReceiver> {
    match VoiceReceiver::new_with_delay_bounds(
        RECEIVER_SAMPLE_RATE_HZ,
        RECEIVER_MIN_DELAY_MS,
        max_delay_ms.max(RECEIVER_MIN_DELAY_MS),
    ) {
        Ok(receiver) => Some(receiver),
        Err(err) => {
            godot_error!("AudioStreamNetwork failed to build VoiceReceiver: {}", err);
            None
        }
    }
}

fn zero_frames(frames: &mut [AudioFrame]) {
    for frame in frames {
        frame.left = 0.0;
        frame.right = 0.0;
    }
}

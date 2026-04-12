use std::slice;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use godot::builtin::{GString, PackedByteArray, VarDictionary};
use godot::classes::native::AudioFrame;
use godot::classes::{AudioStream, AudioStreamPlayback, IAudioStream, IAudioStreamPlayback, Time};
use godot::obj::Gd;
use godot::prelude::*;
use voice_core::{PacketArrival, VoicePacket};

use crate::packet_bytes::decode_packet_bytes;

const DEFAULT_QUEUE_CAPACITY: usize = 256;
const PLAYBACK_DRAIN_BUDGET: usize = 32;

#[derive(Debug)]
struct QueuedPacket {
    packet: VoicePacket,
    arrival: PacketArrival,
}

#[derive(Debug)]
struct SharedStreamState {
    queue: ArrayQueue<QueuedPacket>,
    dropped_packets: AtomicU64,
    current_buffer_size_ms: AtomicU32,
    target_delay_ms: AtomicU32,
    preferred_buffer_size_ms: AtomicU32,
    packets_awaiting_decode: AtomicU32,
    expand_rate: AtomicU32,
    accelerate_rate: AtomicU32,
    concealed_samples: AtomicU64,
    consecutive_failures: AtomicU32,
    playing: AtomicI32,
}

impl SharedStreamState {
    fn new() -> Self {
        Self {
            queue: ArrayQueue::new(DEFAULT_QUEUE_CAPACITY),
            dropped_packets: AtomicU64::new(0),
            current_buffer_size_ms: AtomicU32::new(0),
            target_delay_ms: AtomicU32::new(0),
            preferred_buffer_size_ms: AtomicU32::new(0),
            packets_awaiting_decode: AtomicU32::new(0),
            expand_rate: AtomicU32::new(0),
            accelerate_rate: AtomicU32::new(0),
            concealed_samples: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            playing: AtomicI32::new(0),
        }
    }

    fn reset_runtime_stats(&self) {
        self.current_buffer_size_ms.store(0, Ordering::Relaxed);
        self.target_delay_ms.store(0, Ordering::Relaxed);
        self.preferred_buffer_size_ms.store(0, Ordering::Relaxed);
        self.packets_awaiting_decode.store(0, Ordering::Relaxed);
        self.expand_rate.store(0, Ordering::Relaxed);
        self.accelerate_rate.store(0, Ordering::Relaxed);
        self.concealed_samples.store(0, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
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
#[class(base=AudioStreamPlayback)]
struct AudioStreamNetworkPlayback {
    base: Base<AudioStreamPlayback>,
    shared: Arc<SharedStreamState>,
    playback_position_frames: u64,
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
        let playback = Gd::from_init_fn(move |base| AudioStreamNetworkPlayback {
            base,
            shared,
            playback_position_frames: 0,
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
        self.push_packet_with_meta(bytes, mono_time_now_us() as i64)
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
            "expand_rate",
            self.shared.expand_rate.load(Ordering::Relaxed),
        );
        dict.set(
            "accelerate_rate",
            self.shared.accelerate_rate.load(Ordering::Relaxed),
        );
        dict.set(
            "concealed_samples",
            self.shared.concealed_samples.load(Ordering::Relaxed) as i64,
        );
        dict.set(
            "consecutive_failures",
            self.shared.consecutive_failures.load(Ordering::Relaxed),
        );
        dict.set("queued_packets", self.shared.queue.len() as i64);
        dict.set(
            "dropped_packets",
            self.shared.dropped_packets.load(Ordering::Relaxed) as i64,
        );
        dict.set("configured_max_delay_ms", self.max_delay_ms);
        dict.set(
            "is_playing",
            self.shared.playing.load(Ordering::Relaxed) != 0,
        );
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
impl IAudioStreamPlayback for AudioStreamNetworkPlayback {
    fn init(base: Base<AudioStreamPlayback>) -> Self {
        Self {
            base,
            shared: Arc::new(SharedStreamState::new()),
            playback_position_frames: 0,
        }
    }

    fn start(&mut self, _from_pos: f64) {
        self.playback_position_frames = 0;
        self.shared.reset_runtime_stats();
        self.shared.playing.store(1, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        self.shared.reset_runtime_stats();
        self.shared.playing.store(0, Ordering::Relaxed);
    }

    fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed) != 0
    }

    fn get_playback_position(&self) -> f64 {
        self.playback_position_frames as f64 / 48_000.0
    }

    unsafe fn mix_rawptr(&mut self, buffer: *mut AudioFrame, _rate_scale: f32, frames: i32) -> i32 {
        if buffer.is_null() || frames <= 0 || !self.is_playing() {
            return 0;
        }

        let frame_count = frames as usize;
        let out = unsafe { slice::from_raw_parts_mut(buffer, frame_count) };

        let mut drained = 0;
        while drained < PLAYBACK_DRAIN_BUDGET {
            match self.shared.queue.pop() {
                Some(queued) => {
                    let _ = queued.packet;
                    let _ = queued.arrival;
                    drained += 1;
                }
                None => break,
            }
        }

        for frame in out.iter_mut() {
            frame.left = 0.0;
            frame.right = 0.0;
        }

        self.shared
            .packets_awaiting_decode
            .store(self.shared.queue.len() as u32, Ordering::Relaxed);
        self.playback_position_frames = self
            .playback_position_frames
            .saturating_add(frame_count as u64);

        frames
    }
}

fn mono_time_now_us() -> u64 {
    Time::singleton().get_ticks_usec()
}

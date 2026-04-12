use godot::builtin::{PackedByteArray, PackedFloat32Array, VarDictionary};
use godot::classes::{AudioServer, INode, Node};
use godot::obj::Singleton;
use godot::prelude::*;
use std::collections::VecDeque;
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
    encoder: Option<VoiceEncoder>,
    last_speaking: bool,
    last_error: GString,
    captured_input_frames: i64,
    process_ticks: i64,
    empty_input_polls: i64,
    chunk_count: i64,
    emitted_packets: i64,
    input_samples_pushed: i64,
    max_available_frames: i32,
    max_chunk_frames: i32,
    last_available_frames: i32,
    last_chunk_frames: i32,
    mono_epoch: Instant,
    // Paced-send queue: packets are queued here and drained one-per-20ms in process().
    pending_packets: VecDeque<PackedByteArray>,
    last_paced_emit_us: Option<u64>,
    packets_sent: i64,
    // Stats for the actual emission cadence (updated in drain_paced_queue).
    last_packet_emit_us: Option<u64>,
    packet_interval_count: i64,
    packet_interval_sum_us: u64,
    packet_interval_max_us: u64,
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
            encoder: None,
            last_speaking: false,
            last_error: GString::new(),
            captured_input_frames: 0,
            process_ticks: 0,
            empty_input_polls: 0,
            chunk_count: 0,
            emitted_packets: 0,
            input_samples_pushed: 0,
            max_available_frames: 0,
            max_chunk_frames: 0,
            last_available_frames: 0,
            last_chunk_frames: 0,
            mono_epoch: Instant::now(),
            pending_packets: VecDeque::new(),
            last_paced_emit_us: None,
            packets_sent: 0,
            last_packet_emit_us: None,
            packet_interval_count: 0,
            packet_interval_sum_us: 0,
            packet_interval_max_us: 0,
        }
    }

    fn ready(&mut self) {
        self.rebuild_encoder();
        // Always enable process so drain_paced_queue() runs regardless of capture mode.
        self.base_mut().set_process(true);
        if self.capture_audio_server_input {
            let mut audio_server = AudioServer::singleton();
            let _ = audio_server.set_input_device_active(true);
        }
    }

    fn process(&mut self, _delta: f64) {
        self.process_ticks += 1;
        if self.capture_audio_server_input {
            self.pump_audio_server_input();
        }
        self.drain_paced_queue();
    }

    fn exit_tree(&mut self) {
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
        if let Some(encoder) = self.encoder.as_mut() {
            encoder.flush();
        }
        self.last_speaking = false;
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
        dict.set("emitted_packets", self.emitted_packets);
        dict.set("packets_sent", self.packets_sent);
        dict.set("queued_packets", self.pending_packets.len() as i64);
        dict.set("input_samples_pushed", self.input_samples_pushed);
        dict.set("max_available_frames", self.max_available_frames);
        dict.set("max_chunk_frames", self.max_chunk_frames);
        dict.set("last_available_frames", self.last_available_frames);
        dict.set("last_chunk_frames", self.last_chunk_frames);
        dict.set(
            "capture_audio_server_input",
            self.capture_audio_server_input,
        );
        dict.set(
            "avg_packet_interval_ms",
            if self.packet_interval_count > 0 {
                self.packet_interval_sum_us as f64 / self.packet_interval_count as f64 / 1000.0
            } else {
                0.0
            },
        );
        dict.set(
            "max_packet_interval_ms",
            self.packet_interval_max_us as f64 / 1000.0,
        );
        dict
    }
}

impl NetworkAudioSender {
    fn rebuild_encoder(&mut self) {
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

        match VoiceEncoder::new(config) {
            Ok(encoder) => {
                self.encoder = Some(encoder);
                self.last_error = GString::new();
            }
            Err(err) => {
                let message = GString::from(err.to_string().as_str());
                self.encoder = None;
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
            self.push_pcm_slice(&mono_samples);
            available -= stereo.len() as i32;
        }
    }

    fn push_pcm_slice(&mut self, samples: &[f32]) -> i32 {
        if self.encoder.is_none() {
            self.rebuild_encoder();
        }

        if self.encoder.is_none() {
            return 0;
        }

        self.input_samples_pushed += samples.len() as i64;
        {
            let encoder = self.encoder.as_mut().expect("encoder checked above");
            encoder.set_force_transmit(self.push_to_talk);
            encoder.push_pcm(samples);
        }

        let mut emitted = 0;
        loop {
            let polled = {
                let encoder = self.encoder.as_mut().expect("encoder checked above");
                encoder.poll_packet()
            };

            match polled {
                Ok(Some(packet)) => {
                    self.emitted_packets += 1;
                    self.last_speaking = !packet.flags.contains(PacketFlags::END_OF_TALKSPURT);
                    let packet_bytes = encode_packet_bytes(&packet);
                    // Queue for paced emission; drop tail if the queue is full.
                    if self.pending_packets.len() < MAX_PENDING_PACKETS {
                        self.pending_packets.push_back(packet_bytes);
                    }
                    emitted += 1;
                }
                Ok(None) => break,
                Err(err) => {
                    let message = GString::from(err.to_string().as_str());
                    self.last_error = message.clone();
                    self.base_mut()
                        .emit_signal("encoder_error", &[message.to_variant()]);
                    break;
                }
            }
        }

        emitted
    }

    /// Drain the pending-packet queue at a paced rate of one packet per 20 ms.
    ///
    /// Called every `process()` tick. Uses a virtual-schedule clock so the
    /// average inter-packet interval converges to 20 ms even when `process()`
    /// fires at a different rate (e.g. 60 fps ≈ 16.7 ms or 8 fps headless).
    ///
    /// If the virtual schedule has fallen more than 10 s behind (app was
    /// paused / minimized) we reset it to now so we don't burst through
    /// a multi-second backlog. The pending-packet queue cap (MAX_PENDING_PACKETS)
    /// naturally limits burst size for normal low-fps scenarios.
    fn drain_paced_queue(&mut self) {
        let now_us = self.mono_epoch.elapsed().as_micros() as u64;

        loop {
            // Compute when the next packet slot is due on the virtual schedule.
            let next_due_us = match self.last_paced_emit_us {
                None => now_us, // first ever packet: send immediately
                Some(last) => last + PACE_INTERVAL_US,
            };

            if now_us < next_due_us {
                break; // not yet time for the next packet
            }

            let Some(packet_bytes) = self.pending_packets.pop_front() else {
                break; // nothing queued
            };

            // Update emission-cadence stats.
            if let Some(prev_us) = self.last_packet_emit_us {
                let delta = now_us.saturating_sub(prev_us);
                self.packet_interval_count += 1;
                self.packet_interval_sum_us =
                    self.packet_interval_sum_us.saturating_add(delta);
                self.packet_interval_max_us = self.packet_interval_max_us.max(delta);
            }
            self.last_packet_emit_us = Some(now_us);

            // Advance the virtual schedule by one slot. Only reset to now
            // after a genuine long stall (>10 s) to avoid bursting a huge
            // backlog. Low-fps scenarios (e.g. 8 fps headless) drain multiple
            // slots per tick without triggering this — the queue cap limits
            // the burst to MAX_PENDING_PACKETS packets.
            self.last_paced_emit_us = Some(
                if next_due_us + PACE_INTERVAL_US * 500 < now_us {
                    now_us
                } else {
                    next_due_us
                },
            );

            self.packets_sent += 1;
            self.base_mut()
                .emit_signal("packet_ready", &[packet_bytes.to_variant()]);
        }
    }
}

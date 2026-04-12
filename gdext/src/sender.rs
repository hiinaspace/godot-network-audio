use godot::builtin::{PackedByteArray, PackedFloat32Array};
use godot::classes::{AudioServer, INode, Node};
use godot::obj::Singleton;
use godot::prelude::*;
use voice_core::vad::VadConfig;
use voice_core::{PacketFlags, VoiceEncoder, VoiceEncoderConfig};

use crate::packet_bytes::encode_packet_bytes;

const DEFAULT_MICROPHONE_FRAME_BUDGET: i32 = 960;

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
        }
    }

    fn ready(&mut self) {
        self.rebuild_encoder();
        if self.capture_audio_server_input {
            let mut audio_server = AudioServer::singleton();
            let _ = audio_server.set_input_device_active(true);
            self.base_mut().set_process(true);
        }
    }

    fn process(&mut self, _delta: f64) {
        if self.capture_audio_server_input {
            self.pump_audio_server_input();
        }
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
            return;
        }

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
                    self.last_speaking = !packet.flags.contains(PacketFlags::END_OF_TALKSPURT);
                    let packet_bytes = encode_packet_bytes(&packet);
                    self.base_mut()
                        .emit_signal("packet_ready", &[packet_bytes.to_variant()]);
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
}

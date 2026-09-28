#[derive(Debug, Clone, Copy)]
pub struct VadConfig {
    pub threshold_db: f32,
    pub hangover_frames: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            hangover_frames: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnergyVad {
    config: VadConfig,
    hangover_remaining: u32,
}

impl EnergyVad {
    pub fn new(config: VadConfig) -> Self {
        Self {
            config,
            hangover_remaining: 0,
        }
    }

    pub fn is_voiced(&mut self, mono_samples: &[f32]) -> bool {
        let rms = if mono_samples.is_empty() {
            0.0
        } else {
            let sum_sq: f32 = mono_samples.iter().map(|s| s * s).sum();
            (sum_sq / mono_samples.len() as f32).sqrt()
        };

        let db = if rms <= 1.0e-9 {
            -120.0
        } else {
            20.0 * rms.log10()
        };

        if db >= self.config.threshold_db {
            self.hangover_remaining = self.config.hangover_frames;
            true
        } else if self.hangover_remaining > 0 {
            self.hangover_remaining -= 1;
            true
        } else {
            false
        }
    }

    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.config.threshold_db = threshold_db;
    }
}

/// A conservative microphone gate applied before both Opus and the local
/// viseme tap. The 160 ms hold avoids chopping word endings while a higher
/// opening threshold keeps ordinary room noise out of the stream.
pub struct MicrophoneGate {
    vad: EnergyVad,
}

impl Default for MicrophoneGate {
    fn default() -> Self {
        Self {
            vad: EnergyVad::new(VadConfig {
                threshold_db: -38.0,
                hangover_frames: 8,
            }),
        }
    }
}

impl MicrophoneGate {
    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.vad.set_threshold_db(threshold_db.clamp(-60.0, -20.0));
    }

    pub fn process(&mut self, frame: &mut [f32]) -> bool {
        if self.vad.is_voiced(frame) {
            true
        } else {
            frame.fill(0.0);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MicrophoneGate;

    #[test]
    fn rejects_room_noise_and_holds_word_tail() {
        let mut gate = MicrophoneGate::default();
        let mut noise = vec![0.008_f32; 960]; // approximately -42 dBFS
        assert!(!gate.process(&mut noise));
        assert!(noise.iter().all(|sample| *sample == 0.0));

        let mut voice = vec![0.04_f32; 960]; // approximately -28 dBFS
        assert!(gate.process(&mut voice));
        assert!(voice.iter().all(|sample| *sample > 0.0));
        for _ in 0..8 {
            let mut tail = vec![0.004_f32; 960];
            assert!(gate.process(&mut tail));
        }
        let mut quiet = vec![0.004_f32; 960];
        assert!(!gate.process(&mut quiet));
        assert!(quiet.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn threshold_changes_without_resetting_gate() {
        let mut gate = MicrophoneGate::default();
        gate.set_threshold_db(-50.0);
        let mut soft_voice = vec![0.008_f32; 960];
        assert!(gate.process(&mut soft_voice));
        gate.set_threshold_db(-20.0);
        for _ in 0..8 {
            let mut tail = vec![0.008_f32; 960];
            assert!(gate.process(&mut tail));
        }
        let mut quiet = vec![0.008_f32; 960];
        assert!(!gate.process(&mut quiet));
    }
}

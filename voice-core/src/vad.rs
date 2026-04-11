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
}

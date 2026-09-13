//! Optional, bounded analysis copy. The audio producer never waits for consumers.
use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub const TAP_SAMPLES: usize = 480;
#[derive(Debug)]
pub struct PcmBlock {
    pub samples: [f32; TAP_SAMPLES],
    pub len: usize,
    pub sample_rate: u32,
    pub sample_start: u64,
    pub generation: u64,
    pub captured_at: Instant,
}
#[derive(Debug)]
pub struct PcmTap {
    enabled: AtomicBool,
    generation: AtomicU64,
    cursor: AtomicU64,
    dropped: AtomicU64,
    queue: ArrayQueue<PcmBlock>,
}
impl Default for PcmTap {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            cursor: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            queue: ArrayQueue::new(128),
        }
    }
}
impl PcmTap {
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub fn dropped_samples(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    /// Invalidates queued/in-flight results. Consumers discard earlier generations.
    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
    pub fn pop(&self) -> Option<PcmBlock> {
        self.queue.pop()
    }
    pub fn push(&self, samples: &[f32], sample_rate: u32) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let captured_at = Instant::now();
        for chunk in samples.chunks(TAP_SAMPLES) {
            let mut block = PcmBlock {
                samples: [0.0; TAP_SAMPLES],
                len: chunk.len(),
                sample_rate,
                sample_start: self.cursor.fetch_add(chunk.len() as u64, Ordering::Relaxed),
                generation: self.generation(),
                captured_at,
            };
            for (out, &sample) in block.samples.iter_mut().zip(chunk) {
                *out = if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
            if self.queue.push(block).is_err() {
                self.dropped
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_bounded_and_discontinuous() {
        let tap = PcmTap::default();
        tap.push(&[0.5; 960], 48000);
        assert!(tap.pop().is_none());
        tap.enable();
        tap.push(&[0.5; 960], 48000);
        let a = tap.pop().unwrap();
        let b = tap.pop().unwrap();
        assert_eq!((a.len, b.sample_start, b.sample_rate), (480, 480, 48000));
        tap.reset();
        tap.push(&[f32::NAN, 2.0], 44100);
        let c = tap.pop().unwrap();
        assert_ne!(a.generation, c.generation);
        assert_eq!(&c.samples[..2], &[0.0, 1.0]);
        for _ in 0..136 {
            tap.push(&[0.0; 480], 48000);
        }
        assert_eq!(tap.queue.len(), 128);
        assert_eq!(tap.dropped_samples(), 8 * 480);
    }
}

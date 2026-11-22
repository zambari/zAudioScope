//! Lock-free L/R rings written by the input callback: the phase strip's
//! short history, and the feed the monitor output plays from.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// L/R frames packed `L << 32 | R` into atomics. One writer (the input
/// callback) stores frames, then publishes the count with `Release`.
pub struct StereoRing {
    buf: Box<[AtomicU64]>,
    written: AtomicUsize,
}

impl StereoRing {
    pub fn new(len: usize) -> Self {
        Self { buf: (0..len.max(2)).map(|_| AtomicU64::new(0)).collect(), written: AtomicUsize::new(0) }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Writer side.
    pub fn write(&self, at: usize, l: f32, r: f32) {
        let packed = (l.to_bits() as u64) << 32 | r.to_bits() as u64;
        self.buf[at % self.buf.len()].store(packed, Ordering::Relaxed);
    }

    pub fn publish(&self, written: usize) {
        self.written.store(written, Ordering::Release);
    }

    /// Frames ever written; the writer's own count when called from it.
    pub fn written(&self) -> usize {
        self.written.load(Ordering::Acquire)
    }

    /// Frame `i` (absolute index); valid while `written - len < i < written`.
    pub fn get(&self, i: usize) -> (f32, f32) {
        let v = self.buf[i % self.buf.len()].load(Ordering::Relaxed);
        (f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32))
    }

    /// The last `n` frames, oldest first.
    pub fn latest(&self, n: usize) -> Vec<(f32, f32)> {
        let w = self.written();
        let n = n.min(self.buf.len() / 2).min(w);
        (w - n..w).map(|i| self.get(i)).collect()
    }
}

/// Room for the largest input callback plus output-side jitter.
const MONITOR_LEN: usize = 1 << 15;

/// The monitor feed: the selected input channels after input gain. Lives
/// as long as the app, across input reopens, so the output side never has
/// to re-find it.
pub struct MonitorTap {
    pub ring: StereoRing,
    /// The input callback only fills the ring while this is set.
    pub on: AtomicBool,
    /// Input stream rate and frames per callback, for the output's resampler.
    pub rate: AtomicU32,
    pub block: AtomicU32,
}

impl Default for MonitorTap {
    fn default() -> Self {
        Self {
            ring: StereoRing::new(MONITOR_LEN),
            on: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            block: AtomicU32::new(0),
        }
    }
}

impl MonitorTap {
    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// End of one input callback's writes.
    pub fn publish(&self, written: usize, rate: u32, block: u32) {
        self.rate.store(rate, Ordering::Relaxed);
        self.block.store(block, Ordering::Relaxed);
        self.ring.publish(written);
    }
}

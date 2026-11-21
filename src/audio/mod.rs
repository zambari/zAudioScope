//! Audio input: device listing, one capture stream, and a mono ring buffer
//! the analyser modules read from on the UI thread.

pub mod input;
pub mod spectrum;

pub use input::{DeviceInfo, Input};

/// What the modules get each frame: the most recent samples (mono, oldest
/// first) and where they sit in the stream.
pub struct Frame {
    pub samples: Vec<f32>,
    pub rate: u32,
    /// Absolute index (since the stream opened) of `samples[0]`.
    pub start: u64,
}

impl Frame {
    pub fn ms_to_samples(&self, ms: f32) -> usize {
        (ms * self.rate as f32 / 1000.0).max(0.0) as usize
    }
}

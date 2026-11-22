//! Audio input: device listing, one capture stream, and a mono ring buffer
//! the analyser modules read from on the UI thread.

/// Call a generic stream builder `f::<T>(args)` with the sample type the
/// device runs at.
macro_rules! by_format {
    ($fmt:expr, $f:ident($($arg:expr),*)) => {
        match $fmt {
            cpal::SampleFormat::F32 => $f::<f32>($($arg),*),
            cpal::SampleFormat::F64 => $f::<f64>($($arg),*),
            cpal::SampleFormat::I16 => $f::<i16>($($arg),*),
            cpal::SampleFormat::I32 => $f::<i32>($($arg),*),
            cpal::SampleFormat::U16 => $f::<u16>($($arg),*),
            other => Err(format!("unsupported device sample format {other}")),
        }
    };
}
pub(crate) use by_format;

pub mod input;
pub mod levels;
pub mod loudness;
pub mod mic_permission;
pub mod monitor;
pub mod spectrum;
pub mod tap;

pub use monitor::{Monitor, MonitorSettings};
pub use input::{ChannelSel, DeviceInfo, HISTORY_CHOICES, Input, InputSettings};

/// What the modules get each frame: the most recent samples (mono, oldest
/// first) and where they sit in the stream.
pub struct Frame {
    pub samples: Vec<f32>,
    pub rate: u32,
    /// Absolute index (since the stream opened) of `samples[0]`.
    pub start: u64,
}

impl Frame {
    /// Absolute index one past the newest sample.
    pub fn end(&self) -> u64 {
        self.start + self.samples.len() as u64
    }

    pub fn ms_to_samples(&self, ms: f32) -> usize {
        (ms * self.rate as f32 / 1000.0).max(0.0) as usize
    }
}

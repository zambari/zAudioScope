use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use super::Frame;

/// Seconds of history kept in the ring.
const HISTORY_SECS: u32 = 4;

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// Mono samples written by the audio callback, read by the UI.
struct Ring {
    buf: Vec<f32>,
    /// Total samples ever written; `written % buf.len()` is the write head.
    written: u64,
    /// Peak since the UI last looked, for the level meter.
    peak: f32,
}

impl Ring {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], written: 0, peak: 0.0 }
    }

    fn push(&mut self, s: f32) {
        let len = self.buf.len() as u64;
        self.buf[(self.written % len) as usize] = s;
        self.written += 1;
        self.peak = self.peak.max(s.abs());
    }

    /// The last `n` samples, oldest first, and the absolute index of the first.
    fn latest(&self, n: usize) -> (Vec<f32>, u64) {
        let len = self.buf.len();
        let n = n.min(len).min(self.written as usize);
        let start = self.written - n as u64;
        let out = (0..n as u64).map(|i| self.buf[((start + i) % len as u64) as usize]).collect();
        (out, start)
    }
}

struct Shared {
    ring: Mutex<Ring>,
    error: Mutex<Option<String>>,
}

pub struct Input {
    stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    pub device_id: Option<String>,
    pub device_name: String,
    pub rate: u32,
    pub channels: u16,
    open_error: Option<String>,
}

impl Input {
    pub fn open(device_id: Option<&str>) -> Self {
        let mut me = Self {
            stream: None,
            shared: Arc::new(Shared { ring: Mutex::new(Ring::new(1)), error: Mutex::new(None) }),
            device_id: None,
            device_name: "none".into(),
            rate: 48_000,
            channels: 0,
            open_error: None,
        };
        me.switch(device_id);
        me
    }

    pub fn devices() -> Vec<DeviceInfo> {
        let host = cpal::default_host();
        let Ok(devs) = host.input_devices() else {
            return Vec::new();
        };
        devs.filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let name = d.description().map(|x| x.name().to_owned()).unwrap_or_else(|_| id.clone());
            Some(DeviceInfo { id, name })
        })
        .collect()
    }

    /// Close the current stream and open another device (`None`: system default).
    pub fn switch(&mut self, device_id: Option<&str>) {
        self.stream = None;
        self.open_error = self.open_stream(device_id).err();
    }

    fn open_stream(&mut self, device_id: Option<&str>) -> Result<(), String> {
        let host = cpal::default_host();
        let device = device_id
            .and_then(|id| id.parse::<cpal::DeviceId>().ok())
            .and_then(|id| host.device_by_id(&id))
            .or_else(|| host.default_input_device())
            .ok_or("no input device")?;
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        self.device_id = device.id().ok().map(|id| id.to_string());
        self.device_name =
            device.description().map(|d| d.name().to_owned()).unwrap_or_else(|_| "input".to_owned());
        self.rate = config.sample_rate();
        self.channels = config.channels();

        self.shared = Arc::new(Shared {
            ring: Mutex::new(Ring::new((self.rate * HISTORY_SECS) as usize)),
            error: Mutex::new(None),
        });

        let sc: cpal::StreamConfig = config.config();
        let sh = self.shared.clone();
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, sc, sh),
            cpal::SampleFormat::F64 => build::<f64>(&device, sc, sh),
            cpal::SampleFormat::I16 => build::<i16>(&device, sc, sh),
            cpal::SampleFormat::I32 => build::<i32>(&device, sc, sh),
            cpal::SampleFormat::U16 => build::<u16>(&device, sc, sh),
            other => return Err(format!("unsupported device sample format {other}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(())
    }

    /// The last `n` samples for the modules to analyse.
    pub fn frame(&self, n: usize) -> Frame {
        let (samples, start) = self.shared.ring.lock().expect("poisoned").latest(n);
        Frame { samples, rate: self.rate, start }
    }

    /// Peak level since the last call, 0..1.
    pub fn take_peak(&self) -> f32 {
        std::mem::take(&mut self.shared.ring.lock().expect("poisoned").peak)
    }

    pub fn error(&self) -> Option<String> {
        self.open_error.clone().or_else(|| self.shared.error.lock().ok()?.clone())
    }
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, sh: Arc<Shared>) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = config.channels.max(1) as usize;
    let err_sh = sh.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Mix to mono. A later version may let the user pick a channel.
                let Ok(mut ring) = sh.ring.lock() else { return };
                for frame in data.chunks_exact(ch) {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    ring.push(sum / ch as f32);
                }
            },
            move |err| {
                if let Ok(mut e) = err_sh.error.lock() {
                    *e = Some(err.to_string());
                }
            },
            None,
        )
        .map_err(|e| e.to_string())
}

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use serde::{Deserialize, Serialize};

use super::Frame;

/// Capture lengths offered, seconds. The ring is rounded up to a power of
/// two samples, so the actual length is a little longer.
pub const HISTORY_CHOICES: [u32; 6] = [2, 4, 8, 16, 32, 60];
const DEFAULT_HISTORY_SECS: u32 = 4;
/// L/R frames kept for the phase strip.
const STEREO_LEN: usize = 4096;
/// Rates offered in the picker when the device supports them.
const STANDARD_RATES: [u32; 7] = [32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// Buffer sizes (frames) offered when the device's range allows them.
const STANDARD_BUFFERS: [u32; 9] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096];

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// Which device channels feed the analysers. Channel numbers are 0-based
/// here and 1-based in the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ChannelSelRepr", into = "ChannelSelRepr")]
pub enum ChannelSel {
    /// One channel.
    Mono(u16),
    /// Channels `c` and `c + 1` as L/R; analysers get their average.
    Pair(u16),
}

impl Default for ChannelSel {
    fn default() -> Self {
        Self::Pair(0)
    }
}

/// Saved form; keeps the old `Mix` so earlier saved states still load.
#[derive(Clone, Copy, Serialize, Deserialize)]
enum ChannelSelRepr {
    Mix,
    Mono(u16),
    Pair(u16),
}

impl From<ChannelSelRepr> for ChannelSel {
    fn from(r: ChannelSelRepr) -> Self {
        match r {
            ChannelSelRepr::Mix => Self::Pair(0),
            ChannelSelRepr::Mono(c) => Self::Mono(c),
            ChannelSelRepr::Pair(c) => Self::Pair(c),
        }
    }
}

impl From<ChannelSel> for ChannelSelRepr {
    fn from(s: ChannelSel) -> Self {
        match s {
            ChannelSel::Mono(c) => Self::Mono(c),
            ChannelSel::Pair(c) => Self::Pair(c),
        }
    }
}

impl ChannelSel {
    pub fn label(self) -> String {
        match self {
            Self::Mono(c) => format!("Ch {}", c + 1),
            Self::Pair(c) => format!("Ch {}/{}", c + 1, c + 2),
        }
    }

    /// Whether the selection exists on a device with `channels` channels.
    pub fn fits(self, channels: u16) -> bool {
        match self {
            Self::Mono(c) => c < channels,
            Self::Pair(c) => c + 1 < channels,
        }
    }

    fn to_bits(self) -> u32 {
        match self {
            Self::Mono(c) => 0x1_0000 | c as u32,
            Self::Pair(c) => 0x2_0000 | c as u32,
        }
    }

    fn from_bits(b: u32) -> Self {
        let c = (b & 0xFFFF) as u16;
        match b >> 16 {
            1 => Self::Mono(c),
            _ => Self::Pair(c),
        }
    }
}

/// Mono samples written by the audio callback, read by the UI, with no
/// lock: the callback never waits, never defers, never drops. Samples are
/// `f32` bits in atomics; `written` is published once per callback with
/// `Release`, and a reader copies behind it, then checks that the writer
/// hasn't lapped what it copied.
struct Ring {
    buf: Box<[AtomicU32]>,
    /// Total samples ever written; `written % buf.len()` is the write head.
    written: AtomicU64,
    /// Per-channel peak since the UI last looked, for the level meter.
    /// Non-negative `f32` bits order like the floats, so `fetch_max` works.
    peaks: [AtomicU32; 2],
    /// The selected L/R pair (or the mono channel twice), packed L<<32 | R,
    /// for the phase strip.
    stereo: Box<[AtomicU64]>,
    stereo_written: AtomicUsize,
    /// Every device channel's peak since the UI last looked; only filled
    /// while the channel picker is open.
    all_peaks: Box<[AtomicU32]>,
}

fn atomics<A>(n: usize, f: impl Fn() -> A) -> Box<[A]> {
    (0..n).map(|_| f()).collect()
}

impl Ring {
    fn new(len: usize, channels: u16) -> Self {
        Self {
            buf: atomics(len.max(1), || AtomicU32::new(0)),
            written: AtomicU64::new(0),
            peaks: [AtomicU32::new(0), AtomicU32::new(0)],
            stereo: atomics(STEREO_LEN, || AtomicU64::new(0)),
            stereo_written: AtomicUsize::new(0),
            all_peaks: atomics(channels as usize, || AtomicU32::new(0)),
        }
    }

    /// Writer side; only the audio callback calls these.
    fn write(&self, at: u64, s: f32, stereo_at: usize, l: f32, r: f32) {
        self.buf[(at % self.buf.len() as u64) as usize].store(s.to_bits(), Ordering::Relaxed);
        let packed = (l.to_bits() as u64) << 32 | r.to_bits() as u64;
        self.stereo[stereo_at % STEREO_LEN].store(packed, Ordering::Relaxed);
    }

    fn publish(&self, written: u64, stereo_written: usize) {
        self.stereo_written.store(stereo_written, Ordering::Release);
        self.written.store(written, Ordering::Release);
    }

    fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// The last `n` L/R frames, oldest first.
    fn latest_stereo(&self, n: usize) -> Vec<(f32, f32)> {
        let w = self.stereo_written.load(Ordering::Acquire);
        let n = n.min(STEREO_LEN / 2).min(w);
        (w - n..w)
            .map(|i| {
                let v = self.stereo[i % STEREO_LEN].load(Ordering::Relaxed);
                (f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32))
            })
            .collect()
    }

    /// The last `n` samples, oldest first, and the absolute index of the first.
    /// `slack`: samples the writer may be past `written` mid-callback; any
    /// copied sample that close to being overwritten is dropped from the front.
    fn latest(&self, n: usize, slack: u64) -> (Vec<f32>, u64) {
        let len = self.buf.len() as u64;
        let end = self.written();
        let n = (n as u64).min(len.saturating_sub(slack)).min(end);
        let start = end - n;
        let out: Vec<f32> =
            (start..end).map(|i| f32::from_bits(self.buf[(i % len) as usize].load(Ordering::Relaxed))).collect();
        // If the writer lapped the oldest samples while we copied (the UI
        // thread stalled for most of the ring's length), drop them.
        let safe = (self.written() + slack).saturating_sub(len);
        let skip = safe.saturating_sub(start).min(n) as usize;
        (out[skip..].to_vec(), start + skip as u64)
    }
}

struct Shared {
    ring: Ring,
    error: Mutex<Option<String>>,
    /// Buffer under/overruns reported by the driver since the stream opened.
    xruns: AtomicU64,
    /// Frames in the most recent callback: the buffer size actually in use.
    callback_frames: AtomicU32,
    /// Times audio stopped arriving for much longer than one buffer, e.g.
    /// macOS pausing the IO (no mic permission, device grabbed elsewhere).
    gaps: AtomicU64,
    /// Frames of driver-filled zeros left out of the ring (see `build`).
    skipped: AtomicU64,
    /// Set by the error callback on an xrun, cleared by the data callback.
    xrun_flag: AtomicBool,
}

/// Input choices that survive a restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSettings {
    pub device_id: Option<String>,
    /// `None`: the device's current/default rate.
    pub rate: Option<u32>,
    pub channel: ChannelSel,
    pub paused: bool,
    /// Requested callback size in frames; `None`: the driver's default.
    pub buffer: Option<u32>,
    /// Seconds of audio the capture ring holds, whatever the callback size.
    pub history_secs: u32,
    /// Last channel picked on each device, restored when switching back.
    pub device_channels: BTreeMap<String, ChannelSel>,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            device_id: None,
            rate: None,
            channel: ChannelSel::default(),
            paused: false,
            buffer: None,
            history_secs: DEFAULT_HISTORY_SECS,
            device_channels: BTreeMap::new(),
        }
    }
}

pub struct Input {
    stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    pub device_id: Option<String>,
    pub device_name: String,
    /// The rate the stream runs (or would run) at.
    pub rate: u32,
    /// Channels the device has.
    pub channels: u16,
    /// Channels the stream is opened with: just enough for the selection,
    /// or all of them while the channel picker meters every channel.
    stream_channels: u16,
    /// Reuse the ring on the next open (same device, rate and length), so a
    /// channel-count reopen keeps the captured audio.
    keep_ring: bool,
    /// Standard rates the device supports, ascending.
    pub rates: Vec<u32>,
    /// The device's own default rate.
    pub default_rate: u32,
    want_rate: Option<u32>,
    /// Buffer sizes the device accepts, ascending (empty: not adjustable).
    pub buffer_sizes: Vec<u32>,
    want_buffer: Option<u32>,
    history_secs: u32,
    /// Ask the callback to measure every channel (channel picker open).
    meter_all: Arc<AtomicBool>,
    paused: bool,
    open_error: Option<String>,
    /// Input gain as `f32` bits, applied in the callback so every analyser,
    /// the meter and the trigger all see the same scaled signal.
    gain: Arc<AtomicU32>,
    /// `ChannelSel::to_bits`, read by the callback, so changing channel needs
    /// no reopen.
    channel: Arc<AtomicU32>,
    /// Last channel picked on each device id.
    device_channels: BTreeMap<String, ChannelSel>,
}

impl Input {
    pub fn open(settings: &InputSettings) -> Self {
        let mut me = Self {
            stream: None,
            shared: new_shared(1, 0),
            device_id: None,
            device_name: "none".into(),
            rate: 48_000,
            channels: 0,
            stream_channels: 0,
            keep_ring: false,
            rates: Vec::new(),
            default_rate: 48_000,
            want_rate: settings.rate,
            buffer_sizes: Vec::new(),
            want_buffer: settings.buffer,
            history_secs: settings.history_secs.clamp(1, 600),
            meter_all: Arc::new(AtomicBool::new(false)),
            paused: settings.paused,
            open_error: None,
            gain: Arc::new(AtomicU32::new(1f32.to_bits())),
            channel: Arc::new(AtomicU32::new(settings.channel.to_bits())),
            device_channels: settings.device_channels.clone(),
        };
        me.switch(settings.device_id.as_deref());
        me
    }

    pub fn settings(&self) -> InputSettings {
        InputSettings {
            device_id: self.device_id.clone(),
            rate: self.want_rate,
            channel: self.channel(),
            paused: self.paused,
            buffer: self.want_buffer,
            history_secs: self.history_secs,
            device_channels: self.device_channels.clone(),
        }
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

    /// Close the current stream and open another device (`None`: system
    /// default). While paused, only looks the device up.
    pub fn switch(&mut self, device_id: Option<&str>) {
        self.stream = None;
        self.open_error = self.open_stream(device_id).err();
    }

    /// Switch to another device picked by the user, restoring the channel
    /// last used on it (the current device's pick is remembered first).
    pub fn select_device(&mut self, device_id: &str) {
        self.remember_channel();
        if let Some(&sel) = self.device_channels.get(device_id) {
            self.channel.store(sel.to_bits(), Ordering::Relaxed);
        }
        self.switch(Some(device_id));
    }

    fn remember_channel(&mut self) {
        if let Some(id) = &self.device_id {
            self.device_channels.insert(id.clone(), self.channel());
        }
    }

    /// Reopen the current device at `rate` (`None`: its default).
    pub fn set_rate(&mut self, rate: Option<u32>) {
        self.want_rate = rate;
        let id = self.device_id.clone();
        self.switch(id.as_deref());
    }

    /// Reopen the current device with a callback of `frames` (`None`: default).
    pub fn set_buffer(&mut self, frames: Option<u32>) {
        self.want_buffer = frames;
        let id = self.device_id.clone();
        self.switch(id.as_deref());
    }

    pub fn buffer(&self) -> Option<u32> {
        self.want_buffer
    }

    pub fn history_secs(&self) -> u32 {
        self.history_secs
    }

    /// Change the capture length. Reallocates the ring, so the stream is
    /// reopened and the captured audio starts over.
    pub fn set_history_secs(&mut self, secs: u32) {
        self.history_secs = secs.clamp(1, 600);
        let id = self.device_id.clone();
        self.switch(id.as_deref());
    }

    /// Samples the ring holds, and how many of them are filled so far.
    /// `filled == capacity`: a full capture's worth is available.
    pub fn captured(&self) -> (usize, usize) {
        let ring = &self.shared.ring;
        (ring.buf.len(), (ring.written() as usize).min(ring.buf.len()))
    }

    /// Frames per callback as delivered, 0 before the first one.
    pub fn callback_frames(&self) -> u32 {
        self.shared.callback_frames.load(Ordering::Relaxed)
    }

    /// Measure every channel's level. Opens the stream with every channel,
    /// so only while someone looks at it (the channel picker).
    pub fn set_meter_all(&mut self, on: bool) {
        if self.meter_all.swap(on, Ordering::Relaxed) != on {
            self.sync_channels();
        }
    }

    /// Channels the stream needs: up to the selected one, or all of them
    /// while metering. Never more than the device has.
    fn wanted_channels(&self) -> u16 {
        let need = if self.meter_all.load(Ordering::Relaxed) {
            self.channels
        } else {
            match self.channel() {
                ChannelSel::Mono(c) => c + 1,
                ChannelSel::Pair(c) => c + 2,
            }
        };
        need.clamp(1, self.channels.max(1))
    }

    /// Reopen the running stream if it carries a different channel count
    /// than the selection needs, keeping the captured audio.
    fn sync_channels(&mut self) {
        if self.paused || self.stream.is_none() || self.wanted_channels() == self.stream_channels {
            return;
        }
        self.keep_ring = true;
        let id = self.device_id.clone();
        self.switch(id.as_deref());
    }

    /// Every channel's peak since the last call, after gain.
    pub fn take_all_peaks(&self) -> Vec<f32> {
        self.shared.ring.all_peaks.iter().map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed))).collect()
    }

    pub fn channel(&self) -> ChannelSel {
        ChannelSel::from_bits(self.channel.load(Ordering::Relaxed))
    }

    pub fn set_channel(&mut self, sel: ChannelSel) {
        self.channel.store(sel.to_bits(), Ordering::Relaxed);
        self.remember_channel();
        self.sync_channels();
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Pausing closes the stream, releasing the interface for other apps
    /// (e.g. a DAW in exclusive mode). The last captured audio stays put.
    pub fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.stream = None;
        } else {
            let id = self.device_id.clone();
            self.switch(id.as_deref());
        }
    }

    fn open_stream(&mut self, device_id: Option<&str>) -> Result<(), String> {
        let host = cpal::default_host();
        let device = device_id
            .and_then(|id| id.parse::<cpal::DeviceId>().ok())
            .and_then(|id| host.device_by_id(&id))
            .or_else(|| host.default_input_device())
            .ok_or("no input device")?;
        self.device_id = device.id().ok().map(|id| id.to_string());
        self.device_name =
            device.description().map(|d| d.name().to_owned()).unwrap_or_else(|_| "input".to_owned());

        let default = device.default_input_config().map_err(|e| e.to_string())?;
        let ranges: Vec<_> = device.supported_input_configs().map(|r| r.collect()).unwrap_or_default();
        let supports = |c: &cpal::SupportedStreamConfigRange, r: u32| c.min_sample_rate() <= r && r <= c.max_sample_rate();
        self.default_rate = default.sample_rate();
        self.rates = STANDARD_RATES
            .into_iter()
            .chain([self.default_rate])
            .filter(|&r| ranges.iter().any(|c| supports(c, r)) || r == self.default_rate)
            .collect();
        self.rates.sort_unstable();
        self.rates.dedup();

        let rate = self.want_rate.filter(|r| self.rates.contains(r)).unwrap_or(self.default_rate);
        // Every channel the device has at this rate, preferring the default
        // sample format, then f32.
        let config = ranges
            .into_iter()
            .filter(|c| supports(c, rate))
            .max_by_key(|c| {
                (c.channels(), c.sample_format() == default.sample_format(), c.sample_format() == cpal::SampleFormat::F32)
            })
            .filter(|c| c.channels() >= default.channels())
            .and_then(|c| c.try_with_sample_rate(rate))
            .unwrap_or(default);
        self.rate = config.sample_rate();
        self.channels = config.channels();
        self.buffer_sizes = match *config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                STANDARD_BUFFERS.into_iter().filter(|&b| min <= b && b <= max).collect()
            }
            cpal::SupportedBufferSize::Unknown => Vec::new(),
        };
        let len = (self.rate as usize * self.history_secs as usize).next_power_of_two();
        let keep = std::mem::take(&mut self.keep_ring) && {
            let ring = &self.shared.ring;
            ring.buf.len() == len && ring.all_peaks.len() == self.channels as usize
        };
        if !keep {
            self.shared = new_shared(len, self.channels);
        }
        if self.paused {
            return Ok(());
        }

        let mut sc: cpal::StreamConfig = config.config();
        // CoreAudio hands over the first N channels; asking for 2 of 14
        // keeps the per-callback work and copying small.
        sc.channels = self.wanted_channels();
        self.stream_channels = sc.channels;
        if let Some(b) = self.want_buffer.filter(|b| self.buffer_sizes.contains(b)) {
            sc.buffer_size = cpal::BufferSize::Fixed(b);
        }
        let taps = Taps {
            sh: self.shared.clone(),
            gain: self.gain.clone(),
            channel: self.channel.clone(),
            meter_all: self.meter_all.clone(),
        };
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, sc, taps),
            cpal::SampleFormat::F64 => build::<f64>(&device, sc, taps),
            cpal::SampleFormat::I16 => build::<i16>(&device, sc, taps),
            cpal::SampleFormat::I32 => build::<i32>(&device, sc, taps),
            cpal::SampleFormat::U16 => build::<u16>(&device, sc, taps),
            other => return Err(format!("unsupported device sample format {other}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(())
    }

    /// The last `n` samples for the modules to analyse.
    pub fn frame(&self, n: usize) -> Frame {
        // Two callbacks' worth of margin (or a generous guess before the
        // first one) against the writer catching up to the copy.
        let slack = match self.callback_frames() {
            0 => 8192,
            f => 2 * f as u64,
        };
        let (samples, start) = self.shared.ring.latest(n, slack);
        Frame { samples, rate: self.rate, start }
    }

    /// Per-channel (L, R) peak since the last call, after gain. A mono
    /// selection reports the same value for both.
    pub fn take_peaks(&self) -> [f32; 2] {
        self.shared.ring.peaks.each_ref().map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
    }

    /// The last `n` L/R frames, after gain.
    pub fn scope(&self, n: usize) -> Vec<(f32, f32)> {
        self.shared.ring.latest_stereo(n)
    }

    pub fn is_stereo(&self) -> bool {
        match self.channel() {
            ChannelSel::Mono(c) => c >= self.channels && self.channels >= 2,
            ChannelSel::Pair(_) => self.channels >= 2,
        }
    }

    pub fn set_gain(&self, g: f32) {
        self.gain.store(g.to_bits(), Ordering::Relaxed);
    }

    /// Interruptions in the incoming audio since the stream (re)opened.
    pub fn gaps(&self) -> u64 {
        self.shared.gaps.load(Ordering::Relaxed)
    }

    /// Frames of driver-filled zeros left out of the capture.
    pub fn skipped(&self) -> u64 {
        self.shared.skipped.load(Ordering::Relaxed)
    }

    /// Driver-reported dropouts since the stream (re)opened.
    pub fn xruns(&self) -> u64 {
        self.shared.xruns.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.open_error.clone().or_else(|| self.shared.error.lock().ok()?.clone())
    }
}

fn new_shared(len: usize, channels: u16) -> Arc<Shared> {
    Arc::new(Shared {
        ring: Ring::new(len, channels),
        error: Mutex::new(None),
        xruns: AtomicU64::new(0),
        callback_frames: AtomicU32::new(0),
        gaps: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
        xrun_flag: AtomicBool::new(false),
    })
}

/// What the callback shares with the UI.
struct Taps {
    sh: Arc<Shared>,
    gain: Arc<AtomicU32>,
    channel: Arc<AtomicU32>,
    meter_all: Arc<AtomicBool>,
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, taps: Taps) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let Taps { sh, gain, channel, meter_all } = taps;
    let ch = config.channels.max(1) as usize;
    let err_sh = sh.clone();
    let rate = config.sample_rate.max(1) as f32;
    let mut all = vec![0f32; ch];
    let t0 = std::time::Instant::now();
    let mut last_cb: Option<f32> = None;
    // Set after a late callback or an xrun, cleared by the first block with
    // any signal. While set, all-zero blocks are what the driver fills in
    // for audio it didn't get, so they stay out of the ring. A healthy
    // stream never sets it, so real digital silence (a USB synth between
    // hits) is still captured.
    let mut trouble = false;
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let g = f32::from_bits(gain.load(Ordering::Relaxed));
                // A selection the device lacks falls back to channels 1/2.
                let sel = match ChannelSel::from_bits(channel.load(Ordering::Relaxed)) {
                    s if s.fits(ch as u16) => s,
                    _ if ch > 1 => ChannelSel::Pair(0),
                    _ => ChannelSel::Mono(0),
                };
                let metering = meter_all.load(Ordering::Relaxed);
                let frames = data.len() / ch;
                sh.callback_frames.store(frames as u32, Ordering::Relaxed);
                // A silence in delivery far longer than a buffer: IO was
                // paused, not just a late callback.
                let now = t0.elapsed().as_secs_f32();
                if let Some(prev) = last_cb.replace(now) {
                    let late = now - prev;
                    if late > (4.0 * frames as f32 / rate).max(0.05) {
                        sh.gaps.fetch_add(1, Ordering::Relaxed);
                    }
                    if late > 2.5 * frames as f32 / rate {
                        trouble = true;
                    }
                }
                if sh.xrun_flag.swap(false, Ordering::Relaxed) {
                    trouble = true;
                }
                if trouble {
                    if data.iter().all(|&x| f32::from_sample(x) == 0.0) {
                        sh.skipped.fetch_add(frames as u64, Ordering::Relaxed);
                        return;
                    }
                    trouble = false;
                }
                let ring = &sh.ring;
                // Only this callback writes, so the published counts are ours.
                let mut w = ring.written.load(Ordering::Relaxed);
                let mut sw = ring.stereo_written.load(Ordering::Relaxed);
                let mut pk = [0f32; 2];
                for frame in data.chunks_exact(ch) {
                    let at = |i: usize| f32::from_sample(frame[i]) * g;
                    if metering {
                        for (i, p) in all.iter_mut().enumerate() {
                            *p = p.max(at(i).abs());
                        }
                    }
                    let (m, l, r) = match sel {
                        ChannelSel::Mono(c) => {
                            let v = at(c as usize);
                            (v, v, v)
                        }
                        ChannelSel::Pair(c) => {
                            let (l, r) = (at(c as usize), at(c as usize + 1));
                            ((l + r) * 0.5, l, r)
                        }
                    };
                    ring.write(w, m, sw, l, r);
                    w += 1;
                    sw += 1;
                    pk[0] = pk[0].max(l.abs());
                    pk[1] = pk[1].max(r.abs());
                }
                ring.publish(w, sw);
                for (dst, p) in ring.peaks.iter().zip(pk) {
                    dst.fetch_max(p.to_bits(), Ordering::Relaxed);
                }
                if metering {
                    for (dst, src) in ring.all_peaks.iter().zip(all.iter_mut()) {
                        dst.fetch_max(std::mem::take(src).to_bits(), Ordering::Relaxed);
                    }
                }
            },
            move |err: cpal::Error| {
                // Overloads are transient (CoreAudio reports one on many
                // device/rate changes): count them, don't latch an error.
                if matches!(err.kind(), cpal::ErrorKind::Xrun) {
                    err_sh.xruns.fetch_add(1, Ordering::Relaxed);
                    err_sh.xrun_flag.store(true, Ordering::Relaxed);
                } else if let Ok(mut e) = err_sh.error.lock() {
                    *e = Some(err.to_string());
                }
            },
            None,
        )
        .map_err(|e| e.to_string())
}

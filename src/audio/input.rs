use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use serde::{Deserialize, Serialize};

use super::Frame;
use super::endpoints;
use super::tap::{MonitorTap, StereoRing};

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
/// Reconnect delays after a loss, by attempt; the last one repeats.
const BACKOFF_MS: [u64; 4] = [250, 500, 1000, 2000];
/// Timer retries for an ASIO device before waiting for an endpoint change:
/// each try loads the driver, and some show a dialog when the box is gone.
const ASIO_TIMER_RETRIES: u32 = 3;
/// Without endpoint notifications, how often a missing wanted device is
/// looked for while running on the default.
const PRESENCE_POLL: Duration = Duration::from_secs(3);
/// No callback for this long right after opening counts as a stall. Some
/// drivers take a while to start.
const FIRST_CALLBACK_WAIT: Duration = Duration::from_secs(3);
/// Shortest stall that counts once callbacks have been arriving.
const MIN_STALL: Duration = Duration::from_millis(500);

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
    /// The selected L/R pair (or the mono channel twice), for the phase strip.
    stereo: StereoRing,
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
            stereo: StereoRing::new(STEREO_LEN),
            all_peaks: atomics(channels as usize, || AtomicU32::new(0)),
        }
    }

    /// Writer side; only the audio callback calls these.
    fn write(&self, at: u64, s: f32, stereo_at: usize, l: f32, r: f32) {
        self.buf[(at % self.buf.len() as u64) as usize].store(s.to_bits(), Ordering::Relaxed);
        self.stereo.write(stereo_at, l, r);
    }

    fn publish(&self, written: u64, stereo_written: usize) {
        self.stereo.publish(stereo_written);
        self.written.store(written, Ordering::Release);
    }

    fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// The last `n` samples, oldest first, and the absolute index of the first.
    /// `slack`: samples the writer may be past `written` mid-callback; any
    /// copied sample that close to being overwritten is dropped from the front.
    fn latest(&self, n: usize, slack: u64) -> (Vec<f32>, u64) {
        let len = self.buf.len() as u64;
        let end = self.written();
        let n = (n as u64).min(len.saturating_sub(slack)).min(end);
        let start = end - n;
        let out: Vec<f32> = (start..end)
            .map(|i| f32::from_bits(self.buf[(i % len) as usize].load(Ordering::Relaxed)))
            .collect();
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
    /// Callbacks delivered, empty ones included: the stall watchdog's pulse.
    callbacks: AtomicU64,
    /// Set by the error callback when the stream died and must be rebuilt.
    lost: AtomicBool,
}

/// Whether the stream is healthy, or down and being retried.
enum Link {
    Up,
    Lost {
        reason: String,
        attempt: u32,
        retry_at: Instant,
    },
}

fn backoff(attempt: u32) -> Duration {
    let i = (attempt as usize).min(BACKOFF_MS.len() - 1);
    Duration::from_millis(BACKOFF_MS[i])
}

fn is_asio(id: Option<&str>) -> bool {
    id.is_some_and(|id| id.starts_with("asio:"))
}

/// A stream error in words a user can act on.
pub(crate) fn describe_err(e: &cpal::Error) -> String {
    use cpal::ErrorKind as K;
    match e.kind() {
        K::DeviceBusy => "device is in use by another app (exclusive mode?)".into(),
        K::DeviceNotAvailable => "device not available (unplugged or disabled)".into(),
        K::PermissionDenied => "access denied: check the microphone privacy settings".into(),
        K::HostUnavailable => "audio service not running".into(),
        K::StreamInvalidated => "stream invalidated (device format or route changed)".into(),
        _ => e.to_string(),
    }
}

/// What a stream error callback should do about `err`.
pub(crate) enum ErrAction {
    /// A dropout: count it.
    Xrun,
    /// Informational; the stream carries on.
    Ignore,
    /// The stream is dead and must be rebuilt.
    Lost(String),
}

pub(crate) fn classify(err: &cpal::Error) -> ErrAction {
    use cpal::ErrorKind as K;
    match err.kind() {
        K::Xrun => ErrAction::Xrun,
        // DeviceChanged: rerouted and still running. RealtimeDenied: runs
        // anyway, just without the priority boost.
        K::DeviceChanged | K::RealtimeDenied => ErrAction::Ignore,
        _ => ErrAction::Lost(describe_err(err)),
    }
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

/// A running capture, through cpal or DirectSound. Held only to keep it
/// running; dropping it stops it.
#[allow(dead_code)]
enum Capture {
    Cpal(cpal::Stream),
    #[cfg(windows)]
    DirectSound(super::dsound::Capture),
}

pub struct Input {
    stream: Option<Capture>,
    shared: Arc<Shared>,
    /// The device asked for (`None`: system default). Persisted, and kept
    /// while that device is missing, so it comes back when the device does.
    pub device_id: Option<String>,
    /// The device actually open: the default while the wanted one is
    /// missing.
    opened_id: Option<String>,
    /// Running on the default because the wanted device is missing.
    fallback: bool,
    /// When `fallback` last looked for the wanted device.
    presence_checked: Instant,
    /// Health of the stream.
    link: Link,
    /// Endpoint change count last acted on.
    seen_gen: u64,
    /// Stall watchdog: callback count at the last look, and when it moved.
    last_callbacks: u64,
    last_progress: Instant,
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
    /// Released while the app is in the background. Transient: never saved,
    /// and separate from the user's own pause.
    suspended: bool,
    /// Input gain as `f32` bits, applied in the callback so every analyser,
    /// the meter and the trigger all see the same scaled signal.
    gain: Arc<AtomicU32>,
    /// `ChannelSel::to_bits`, read by the callback, so changing channel needs
    /// no reopen.
    channel: Arc<AtomicU32>,
    /// Last channel picked on each device id.
    device_channels: BTreeMap<String, ChannelSel>,
    /// Feed for the monitor output, kept across reopens.
    monitor: Arc<MonitorTap>,
}

impl Input {
    pub fn open(settings: &InputSettings) -> Self {
        let mut me = Self {
            stream: None,
            shared: new_shared(1, 0),
            device_id: None,
            opened_id: None,
            fallback: false,
            presence_checked: Instant::now(),
            link: Link::Up,
            seen_gen: endpoints::generation(),
            last_callbacks: 0,
            last_progress: Instant::now(),
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
            suspended: false,
            gain: Arc::new(AtomicU32::new(1f32.to_bits())),
            channel: Arc::new(AtomicU32::new(settings.channel.to_bits())),
            device_channels: settings.device_channels.clone(),
            monitor: Arc::default(),
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

    /// Every input device, ASIO included. Listing ASIO loads each driver
    /// (some grab the hardware or show dialogs), so this is for startup and
    /// the explicit rescan only.
    pub fn devices() -> Vec<DeviceInfo> {
        #[cfg_attr(not(feature = "asio"), allow(unused_mut))]
        let mut list = Self::system_devices();
        #[cfg(feature = "asio")]
        if let Ok(asio) = cpal::host_from_id(cpal::HostId::Asio) {
            list.extend(asio.input_devices().map(describe).unwrap_or_default());
        }
        list
    }

    /// The list after an endpoint change: system devices rescanned, ASIO
    /// entries carried over from `prev` without touching the drivers.
    pub fn devices_refresh(prev: &[DeviceInfo]) -> Vec<DeviceInfo> {
        let mut list = Self::system_devices();
        list.extend(prev.iter().filter(|d| is_asio(Some(&d.id))).cloned());
        list
    }

    fn system_devices() -> Vec<DeviceInfo> {
        let list: Vec<DeviceInfo> = cpal::default_host()
            .input_devices()
            .map(describe)
            .unwrap_or_default();
        #[cfg(windows)]
        let mut list = list;
        #[cfg(windows)]
        list.extend(super::dsound::devices());
        list
    }

    /// The device actually open, which differs from `device_id` while
    /// falling back to the default.
    pub fn opened_id(&self) -> Option<&str> {
        self.opened_id.as_deref()
    }

    /// Running on the system default because the wanted device is missing.
    pub fn fallback(&self) -> bool {
        self.fallback && !self.paused
    }

    /// The feed the monitor output plays from.
    pub fn monitor_tap(&self) -> Arc<MonitorTap> {
        self.monitor.clone()
    }

    /// Close the current stream and open another device (`None`: system
    /// default). While paused, only looks the device up.
    pub fn switch(&mut self, device_id: Option<&str>) {
        self.device_id = device_id.map(str::to_owned);
        self.attempt(0);
    }

    /// Close and reopen the wanted device; `attempt` counts failures in a
    /// row, for the retry delay.
    fn attempt(&mut self, attempt: u32) {
        self.stream = None;
        // A fresh open supersedes anything latched by the old stream.
        if let Ok(mut e) = self.shared.error.lock() {
            *e = None;
        }
        self.shared.lost.store(false, Ordering::Relaxed);
        self.link = match self.open_stream() {
            Ok(()) => Link::Up,
            Err(reason) => Link::Lost {
                reason,
                attempt,
                retry_at: Instant::now() + backoff(attempt),
            },
        };
        self.last_callbacks = self.shared.callbacks.load(Ordering::Relaxed);
        self.last_progress = Instant::now();
        self.presence_checked = Instant::now();
    }

    /// Reopen after a loss, keeping the captured audio when the format is
    /// unchanged.
    fn retry(&mut self, attempt: u32) {
        self.keep_ring = true;
        self.attempt(attempt);
    }

    fn lose(&mut self, reason: String) {
        self.stream = None;
        self.link = Link::Lost {
            reason,
            attempt: 0,
            retry_at: Instant::now() + backoff(0),
        };
    }

    /// Call once per UI frame: notices a dead or stalled stream, retries
    /// with backoff, and returns to the wanted device once it is back.
    /// Never blocks unless it reopens.
    pub fn tick(&mut self) {
        if self.held() {
            return;
        }
        let generation = endpoints::generation();
        let changed = generation != self.seen_gen;
        self.seen_gen = generation;
        let now = Instant::now();
        match &self.link {
            Link::Up => {
                if self.shared.lost.load(Ordering::Relaxed) {
                    let reason = self
                        .shared
                        .error
                        .lock()
                        .ok()
                        .and_then(|e| e.clone())
                        .unwrap_or_else(|| "stream stopped".into());
                    self.lose(reason);
                } else if self.stream.is_some() && self.stalled(now) {
                    self.lose("no audio arriving: driver stalled or device gone".into());
                } else if self.fallback && self.wanted_present(changed, now) {
                    self.retry(0);
                }
            }
            &Link::Lost {
                attempt, retry_at, ..
            } => {
                let timer = now >= retry_at
                    && (!is_asio(self.device_id.as_deref()) || attempt < ASIO_TIMER_RETRIES);
                if changed || timer {
                    self.retry(attempt + 1);
                }
            }
        }
    }

    /// No callback for much longer than a buffer. Catches drivers that just
    /// stop calling back without reporting an error, which `gaps` can't:
    /// it only counts once a late callback finally arrives.
    fn stalled(&mut self, now: Instant) -> bool {
        let n = self.shared.callbacks.load(Ordering::Relaxed);
        if n != self.last_callbacks {
            self.last_callbacks = n;
            self.last_progress = now;
            return false;
        }
        let limit = match self.callback_frames() {
            0 => FIRST_CALLBACK_WAIT,
            f => MIN_STALL.max(Duration::from_secs_f32(
                10.0 * f as f32 / self.rate.max(1) as f32,
            )),
        };
        now.duration_since(self.last_progress) > limit
    }

    /// While on the default, whether the wanted device is back. Looks on an
    /// endpoint change, or on a timer where there are no notifications.
    /// ASIO is only looked for on a change: looking loads the driver.
    fn wanted_present(&mut self, changed: bool, now: Instant) -> bool {
        let id = self.device_id.as_deref();
        let timer = !endpoints::watched()
            && !is_asio(id)
            && now.duration_since(self.presence_checked) >= PRESENCE_POLL;
        if !(changed || timer) {
            return false;
        }
        self.presence_checked = now;
        by_id(&host_for(id), id).is_some()
    }

    /// True while audio is arriving: not paused, and the stream is up.
    /// A disconnected or stalled interface reads as not connected.
    pub fn connected(&self) -> bool {
        !self.held()
            && self.stream.is_some()
            && matches!(self.link, Link::Up)
            && !self.shared.lost.load(Ordering::Relaxed)
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
        // The device actually in use: while on the default, picks belong to it.
        if let Some(id) = &self.opened_id {
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
        (
            ring.buf.len(),
            (ring.written() as usize).min(ring.buf.len()),
        )
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
        if self.held()
            || self.stream.is_none()
            || !matches!(self.link, Link::Up)
            || self.wanted_channels() == self.stream_channels
        {
            return;
        }
        // DirectSound always captures every channel; nothing to reopen.
        if self.fixed_channels() {
            return;
        }
        self.keep_ring = true;
        let id = self.device_id.clone();
        self.switch(id.as_deref());
    }

    /// Every channel's peak since the last call, after gain.
    pub fn take_all_peaks(&self) -> Vec<f32> {
        self.shared
            .ring
            .all_peaks
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
            .collect()
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

    /// The interface is released, by the user or by the app being in the
    /// background. Nothing opens or reads it meanwhile.
    fn held(&self) -> bool {
        self.paused || self.suspended
    }

    pub fn suspended(&self) -> bool {
        self.suspended
    }

    /// Release the interface while the app is in the background (focus
    /// lost), or take it back on focus. Unlike `set_paused`, this is not
    /// saved: the user's pause choice is untouched.
    pub fn set_suspended(&mut self, suspended: bool) {
        if suspended == self.suspended {
            return;
        }
        self.suspended = suspended;
        if suspended {
            self.stream = None;
            self.link = Link::Up;
        } else if !self.paused {
            let id = self.device_id.clone();
            self.switch(id.as_deref());
        }
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
            self.link = Link::Up;
        } else {
            let id = self.device_id.clone();
            self.switch(id.as_deref());
        }
    }

    /// Whether the open stream always carries every channel (DirectSound).
    fn fixed_channels(&self) -> bool {
        #[cfg(windows)]
        if let Some(Capture::DirectSound(_)) = &self.stream {
            return true;
        }
        false
    }

    fn open_stream(&mut self) -> Result<(), String> {
        let wanted = self.device_id.clone();
        let wanted = wanted.as_deref();
        self.fallback = false;
        #[cfg(windows)]
        if let Some(id) = wanted.filter(|id| id.starts_with(super::dsound::PREFIX)) {
            return self.open_dsound(id);
        }
        let host = host_for(wanted);
        // A wanted device that is missing falls back to the system default,
        // without forgetting it: `tick` switches back once it returns. The
        // default handle follows default-device changes on its own (WASAPI
        // reports one as an invalidated stream, and `tick` reopens).
        let device = match by_id(&host, wanted) {
            Some(d) => d,
            None => {
                let d = cpal::default_host()
                    .default_input_device()
                    .ok_or("no input device")?;
                self.fallback = wanted.is_some();
                d
            }
        };
        self.opened_id = device.id().ok().map(|id| id.to_string());
        self.device_name = device_name(&device, "input");

        let default = device
            .default_input_config()
            .map_err(|e| describe_err(&e))?;
        let ranges: Vec<_> = device
            .supported_input_configs()
            .map(|r| r.collect())
            .unwrap_or_default();
        let supports = |c: &cpal::SupportedStreamConfigRange, r: u32| {
            c.min_sample_rate() <= r && r <= c.max_sample_rate()
        };
        self.default_rate = default.sample_rate();
        self.rates = STANDARD_RATES
            .into_iter()
            .chain([self.default_rate])
            .filter(|&r| ranges.iter().any(|c| supports(c, r)) || r == self.default_rate)
            .collect();
        self.rates.sort_unstable();
        self.rates.dedup();

        let rate = self
            .want_rate
            .filter(|r| self.rates.contains(r))
            .unwrap_or(self.default_rate);
        // Every channel the device has at this rate, preferring the default
        // sample format, then f32.
        let config = ranges
            .into_iter()
            .filter(|c| supports(c, rate))
            .max_by_key(|c| {
                (
                    c.channels(),
                    c.sample_format() == default.sample_format(),
                    c.sample_format() == cpal::SampleFormat::F32,
                )
            })
            .filter(|c| c.channels() >= default.channels())
            .and_then(|c| c.try_with_sample_rate(rate))
            .unwrap_or(default);
        self.rate = config.sample_rate();
        self.channels = config.channels();
        self.buffer_sizes = match *config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => STANDARD_BUFFERS
                .into_iter()
                .filter(|&b| min <= b && b <= max)
                .collect(),
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
        if self.held() {
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
            monitor: self.monitor.clone(),
        };
        let stream = super::by_format!(config.sample_format(), build(&device, sc, taps))?;
        stream.play().map_err(|e| describe_err(&e))?;
        self.stream = Some(Capture::Cpal(stream));
        Ok(())
    }

    /// Open a DirectSound capture. It always runs at the rate and channel
    /// count the device accepted, so there is no rate, buffer or channel
    /// choice beyond that.
    #[cfg(windows)]
    fn open_dsound(&mut self, id: &str) -> Result<(), String> {
        let open = super::dsound::open(id, self.want_rate)?;
        let fmt = open.format;
        self.opened_id = Some(id.to_owned());
        self.device_name = open.name.clone();
        self.default_rate = fmt.rate;
        self.rates = vec![fmt.rate];
        self.rate = fmt.rate;
        self.channels = fmt.channels;
        self.stream_channels = fmt.channels;
        self.buffer_sizes = Vec::new();

        let len = (self.rate as usize * self.history_secs as usize).next_power_of_two();
        let keep = std::mem::take(&mut self.keep_ring) && {
            let ring = &self.shared.ring;
            ring.buf.len() == len && ring.all_peaks.len() == self.channels as usize
        };
        if !keep {
            self.shared = new_shared(len, self.channels);
        }
        if self.held() {
            // Dropping `open` releases the device.
            return Ok(());
        }

        let taps = Taps {
            sh: self.shared.clone(),
            gain: self.gain.clone(),
            channel: self.channel.clone(),
            meter_all: self.meter_all.clone(),
            monitor: self.monitor.clone(),
        };
        let mut pipe = Pipe::new(taps, fmt.rate, fmt.channels);
        let (xrun_sh, err_sh) = (self.shared.clone(), self.shared.clone());
        let capture = super::dsound::start(
            open,
            move |block: &[f32]| pipe.run(block),
            move || {
                xrun_sh.xruns.fetch_add(1, Ordering::Relaxed);
                xrun_sh.xrun_flag.store(true, Ordering::Relaxed);
            },
            move |e: String| {
                if let Ok(mut err) = err_sh.error.lock() {
                    *err = Some(e);
                }
                err_sh.lost.store(true, Ordering::Relaxed);
            },
        );
        self.stream = Some(Capture::DirectSound(capture));
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
        Frame {
            samples,
            rate: self.rate,
            start,
        }
    }

    /// Per-channel (L, R) peak since the last call, after gain. A mono
    /// selection reports the same value for both.
    pub fn take_peaks(&self) -> [f32; 2] {
        self.shared
            .ring
            .peaks
            .each_ref()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
    }

    /// The last `n` L/R frames, after gain.
    pub fn scope(&self, n: usize) -> Vec<(f32, f32)> {
        self.shared.ring.stereo.latest(n)
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

    /// What is wrong, if anything; says so while a reconnect is pending.
    pub fn error(&self) -> Option<String> {
        if self.held() {
            return None;
        }
        match &self.link {
            Link::Lost { reason, .. } => Some(format!("{reason} · reconnecting…")),
            Link::Up => self.shared.error.lock().ok()?.clone(),
        }
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
        callbacks: AtomicU64::new(0),
        lost: AtomicBool::new(false),
    })
}

/// What the callback shares with the UI.
struct Taps {
    sh: Arc<Shared>,
    gain: Arc<AtomicU32>,
    channel: Arc<AtomicU32>,
    meter_all: Arc<AtomicBool>,
    monitor: Arc<MonitorTap>,
}

/// `DeviceInfo` for every device that reports an id.
pub(crate) fn describe(devs: impl Iterator<Item = cpal::Device>) -> Vec<DeviceInfo> {
    devs.filter_map(|d| {
        let id = d.id().ok()?.to_string();
        Some(DeviceInfo {
            name: device_name(&d, &id),
            id,
        })
    })
    .collect()
}

pub(crate) fn device_name(d: &cpal::Device, fallback: &str) -> String {
    d.description()
        .map(|x| x.name().to_owned())
        .unwrap_or_else(|_| fallback.to_owned())
}

/// The host a device id belongs to (`asio:...` lists ASIO), else the
/// default host.
pub(crate) fn host_for(id: Option<&str>) -> cpal::Host {
    id.and_then(|id| id.parse::<cpal::DeviceId>().ok())
        .and_then(|d| cpal::host_from_id(d.host()).ok())
        .unwrap_or_else(cpal::default_host)
}

/// A saved device id, if that device is present.
pub(crate) fn by_id(host: &cpal::Host, id: Option<&str>) -> Option<cpal::Device> {
    host.device_by_id(&id?.parse::<cpal::DeviceId>().ok()?)
}

/// The per-stream half of the capture path: everything the callback keeps
/// between blocks. Shared by cpal callbacks and the DirectSound reader.
struct Pipe {
    taps: Taps,
    ch: usize,
    in_rate: u32,
    /// Per-channel scratch for the meter-all pass.
    all: Vec<f32>,
    t0: std::time::Instant,
    last_cb: Option<f32>,
    /// Set after a late block or an xrun, cleared by the first block with
    /// any signal. While set, all-zero blocks are what the driver fills in
    /// for audio it didn't get, so they stay out of the ring. A healthy
    /// stream never sets it, so real digital silence (a USB synth between
    /// hits) is still captured.
    trouble: bool,
}

impl Pipe {
    fn new(taps: Taps, in_rate: u32, channels: u16) -> Self {
        let ch = channels.max(1) as usize;
        Self {
            taps,
            ch,
            in_rate,
            all: vec![0f32; ch],
            t0: std::time::Instant::now(),
            last_cb: None,
            trouble: false,
        }
    }

    /// Handle one block of interleaved samples.
    fn run<T: Copy>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
        let ch = self.ch;
        let taps = &self.taps;
        let sh = &taps.sh;
        let g = f32::from_bits(taps.gain.load(Ordering::Relaxed));
        // A selection the device lacks falls back to channels 1/2.
        let sel = match ChannelSel::from_bits(taps.channel.load(Ordering::Relaxed)) {
            s if s.fits(ch as u16) => s,
            _ if ch > 1 => ChannelSel::Pair(0),
            _ => ChannelSel::Mono(0),
        };
        let metering = taps.meter_all.load(Ordering::Relaxed);
        sh.callbacks.fetch_add(1, Ordering::Relaxed);
        let frames = data.len() / ch;
        sh.callback_frames.store(frames as u32, Ordering::Relaxed);
        let rate = self.in_rate.max(1) as f32;
        // A silence in delivery far longer than a buffer: IO was
        // paused, not just a late callback.
        let now = self.t0.elapsed().as_secs_f32();
        if let Some(prev) = self.last_cb.replace(now) {
            let late = now - prev;
            if late > (4.0 * frames as f32 / rate).max(0.05) {
                sh.gaps.fetch_add(1, Ordering::Relaxed);
            }
            if late > 2.5 * frames as f32 / rate {
                self.trouble = true;
            }
        }
        if sh.xrun_flag.swap(false, Ordering::Relaxed) {
            self.trouble = true;
        }
        if self.trouble {
            if data.iter().all(|&x| f32::from_sample(x) == 0.0) {
                sh.skipped.fetch_add(frames as u64, Ordering::Relaxed);
                return;
            }
            self.trouble = false;
        }
        let ring = &sh.ring;
        // Only this callback writes, so the published counts are ours.
        let mut w = ring.written.load(Ordering::Relaxed);
        let mut sw = ring.stereo.written();
        let mon = taps.monitor.enabled();
        let mut mw = taps.monitor.ring.written();
        let mut pk = [0f32; 2];
        for frame in data.chunks_exact(ch) {
            let at = |i: usize| f32::from_sample(frame[i]) * g;
            if metering {
                for (i, p) in self.all.iter_mut().enumerate() {
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
            if mon {
                taps.monitor.ring.write(mw, l, r);
                mw += 1;
            }
            w += 1;
            sw += 1;
            pk[0] = pk[0].max(l.abs());
            pk[1] = pk[1].max(r.abs());
        }
        ring.publish(w, sw);
        if mon {
            taps.monitor.publish(mw, self.in_rate, frames as u32);
        }
        for (dst, p) in ring.peaks.iter().zip(pk) {
            dst.fetch_max(p.to_bits(), Ordering::Relaxed);
        }
        if metering {
            for (dst, src) in ring.all_peaks.iter().zip(self.all.iter_mut()) {
                dst.fetch_max(std::mem::take(src).to_bits(), Ordering::Relaxed);
            }
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    taps: Taps,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut pipe = Pipe::new(taps, config.sample_rate, config.channels);
    let err_sh = pipe.taps.sh.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| pipe.run(data),
            move |err: cpal::Error| match classify(&err) {
                // Overloads are transient (CoreAudio reports one on many
                // device/rate changes): count them, don't latch an error.
                ErrAction::Xrun => {
                    err_sh.xruns.fetch_add(1, Ordering::Relaxed);
                    err_sh.xrun_flag.store(true, Ordering::Relaxed);
                }
                ErrAction::Ignore => {}
                // Only flag it here; the UI thread rebuilds the stream.
                // Dropping a stream from its own callback would deadlock.
                ErrAction::Lost(msg) => {
                    if let Ok(mut e) = err_sh.error.lock() {
                        *e = Some(msg);
                    }
                    err_sh.lost.store(true, Ordering::Relaxed);
                }
            },
            None,
        )
        .map_err(|e| describe_err(&e))
}

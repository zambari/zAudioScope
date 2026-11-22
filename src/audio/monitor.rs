//! Monitoring: plays the analysed input out of a chosen output device (e.g.
//! headphones, to check the right source is coming in), with a feedback
//! guard that mutes the output when the input starts running away with it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use serde::{Deserialize, Serialize};

use super::DeviceInfo;
use super::endpoints;
use super::input::{ErrAction, by_id, classify, describe, describe_err, device_name};
use super::tap::MonitorTap;

/// Output callback size asked for, clamped to what the device allows.
const OUT_BUFFER: u32 = 256;
/// Fade-in after opening or re-arming, seconds.
const FADE_IN_S: f32 = 0.5;
/// Ramp to silence once the guard trips, seconds.
const MUTE_S: f32 = 0.02;
/// Reconnect delays after a loss, by attempt; the last one repeats.
const BACKOFF_MS: [u64; 4] = [250, 500, 1000, 2000];

/// What the monitor remembers across restarts. Monitoring itself is never
/// persisted: the app always starts with it off, but keeps the last device.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorSettings {
    /// `None`: the system's default output.
    pub device_id: Option<String>,
    /// Linear.
    pub gain: f32,
    /// Mute on suspected acoustic feedback.
    pub guard: bool,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            device_id: None,
            gain: 0.5,
            guard: true,
        }
    }
}

/// Why the guard muted the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trip {
    /// The input kept getting louder on its own, as a near-pure tone.
    Runaway,
    /// A near-pure tone held at clipping.
    Howl,
}

impl Trip {
    fn from_bits(b: u32) -> Option<Self> {
        match b {
            1 => Some(Self::Runaway),
            2 => Some(Self::Howl),
            _ => None,
        }
    }

    fn bits(self) -> u32 {
        match self {
            Self::Runaway => 1,
            Self::Howl => 2,
        }
    }
}

struct Shared {
    /// User gain, `f32` bits.
    gain: AtomicU32,
    guard: AtomicBool,
    /// The input and output look like the same room (built-in mic into
    /// built-in speakers): trip on less evidence.
    sensitive: AtomicBool,
    /// `Trip::bits`, 0 while armed. Latched until the UI re-arms.
    trip: AtomicU32,
    /// Set by the UI; the callback clears it, resets the guard and fades in.
    rearm: AtomicBool,
    /// Output L/R peak since the UI last looked, `f32` bits.
    peaks: [AtomicU32; 2],
    error: Mutex<Option<String>>,
    /// Set by the error callback when the stream died and must be rebuilt.
    lost: AtomicBool,
}

/// The output went away while monitoring; retried on the same endpoint.
struct Lost {
    reason: String,
    attempt: u32,
    retry_at: Instant,
}

pub struct Monitor {
    stream: Option<cpal::Stream>,
    sh: Arc<Shared>,
    tap: Arc<MonitorTap>,
    enabled: bool,
    /// Chosen output (`None`: the default at the moment monitoring starts).
    pub device_id: Option<String>,
    /// The endpoint actually opened. Reconnects only ever go back to it,
    /// never to whatever is the default by then: after unplugging
    /// headphones that is usually the speakers.
    opened_id: Option<String>,
    pub device_name: String,
    open_error: Option<String>,
    lost: Option<Lost>,
    seen_gen: u64,
}

fn backoff(attempt: u32) -> Duration {
    let i = (attempt as usize).min(BACKOFF_MS.len() - 1);
    Duration::from_millis(BACKOFF_MS[i])
}

impl Monitor {
    pub fn new(settings: &MonitorSettings, tap: Arc<MonitorTap>) -> Self {
        let me = Self {
            stream: None,
            sh: Arc::new(Shared {
                gain: AtomicU32::new(0),
                guard: AtomicBool::new(settings.guard),
                sensitive: AtomicBool::new(false),
                trip: AtomicU32::new(0),
                rearm: AtomicBool::new(false),
                peaks: [AtomicU32::new(0), AtomicU32::new(0)],
                error: Mutex::new(None),
                lost: AtomicBool::new(false),
            }),
            tap,
            enabled: false,
            device_id: settings.device_id.clone(),
            opened_id: None,
            device_name: "default output".into(),
            open_error: None,
            lost: None,
            seen_gen: endpoints::generation(),
        };
        me.set_gain(settings.gain);
        me
    }

    pub fn settings(&self) -> MonitorSettings {
        MonitorSettings {
            device_id: self.device_id.clone(),
            gain: self.gain(),
            guard: self.guard(),
        }
    }

    pub fn devices() -> Vec<DeviceInfo> {
        cpal::default_host()
            .output_devices()
            .map(describe)
            .unwrap_or_default()
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Open or close the output; the input only feeds the tap while on.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.lost = None;
        self.opened_id = None;
        self.drop_stream();
        if let Ok(mut e) = self.sh.error.lock() {
            *e = None;
        }
        let target = self.device_id.clone();
        let result = if on { self.open(target.as_deref()) } else { Ok(()) };
        self.open_error = result.err();
        // A chosen device that is missing now is retried; the default never
        // is, since by the time one appears it may be the speakers.
        if self.open_error.is_some() && self.device_id.is_some() {
            self.opened_id = self.device_id.clone();
            self.lost = Some(Lost {
                reason: self.open_error.take().unwrap_or_default(),
                attempt: 0,
                retry_at: Instant::now() + backoff(0),
            });
        }
    }

    /// Call once per UI frame: notices a dead output and reopens the same
    /// endpoint with backoff. Never falls back to another device.
    pub fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        let generation = endpoints::generation();
        let changed = generation != self.seen_gen;
        self.seen_gen = generation;
        if self.stream.is_some() && self.sh.lost.load(Ordering::Relaxed) {
            let reason = self
                .sh
                .error
                .lock()
                .ok()
                .and_then(|e| e.clone())
                .unwrap_or_else(|| "output stopped".into());
            self.drop_stream();
            self.lost = Some(Lost {
                reason,
                attempt: 0,
                retry_at: Instant::now() + backoff(0),
            });
        }
        let Some(lost) = &self.lost else { return };
        if !(changed || Instant::now() >= lost.retry_at) {
            return;
        }
        let attempt = lost.attempt + 1;
        let target = self.opened_id.clone();
        match self.open(target.as_deref()) {
            Ok(()) => self.lost = None,
            Err(reason) => {
                self.lost = Some(Lost {
                    reason,
                    attempt,
                    retry_at: Instant::now() + backoff(attempt),
                })
            }
        }
    }

    fn drop_stream(&mut self) {
        // Stop feeding the tap first; nothing reads it now.
        self.tap.on.store(false, Ordering::Relaxed);
        self.stream = None;
    }

    pub fn select_device(&mut self, id: &str) {
        self.device_id = Some(id.to_owned());
        if self.enabled {
            self.set_enabled(true);
        }
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.sh.gain.load(Ordering::Relaxed))
    }

    pub fn set_gain(&self, g: f32) {
        self.sh.gain.store(
            g.clamp(0.0, crate::ui::meter::GAIN_MAX).to_bits(),
            Ordering::Relaxed,
        );
    }

    pub fn guard(&self) -> bool {
        self.sh.guard.load(Ordering::Relaxed)
    }

    /// Turning the guard off also lifts a mute it is holding.
    pub fn set_guard(&self, on: bool) {
        self.sh.guard.store(on, Ordering::Relaxed);
        if !on && self.tripped().is_some() {
            self.rearm();
        }
    }

    pub fn set_sensitive(&self, on: bool) {
        self.sh.sensitive.store(on, Ordering::Relaxed);
    }

    pub fn tripped(&self) -> Option<Trip> {
        Trip::from_bits(self.sh.trip.load(Ordering::Relaxed))
    }

    /// Unmute after a trip, fading back in.
    pub fn rearm(&self) {
        self.sh.rearm.store(true, Ordering::Relaxed);
    }

    /// Output (L, R) peak since the last call, as sent to the device.
    pub fn take_peaks(&self) -> [f32; 2] {
        self.sh
            .peaks
            .each_ref()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
    }

    pub fn error(&self) -> Option<String> {
        if let Some(l) = &self.lost {
            return Some(format!("{} · waiting for {}", l.reason, self.device_name));
        }
        self.open_error
            .clone()
            .or_else(|| self.sh.error.lock().ok()?.clone())
    }

    /// Open `id` (`None`: the current default), closing any old stream
    /// first. On success the tap is fed again and the output fades in.
    fn open(&mut self, id: Option<&str>) -> Result<(), String> {
        self.drop_stream();
        if let Ok(mut e) = self.sh.error.lock() {
            *e = None;
        }
        self.sh.lost.store(false, Ordering::Relaxed);
        let host = cpal::default_host();
        // A saved device that is gone is an error, not a fallback: the
        // default is often the speakers, the one place monitoring a mic
        // must not end up by surprise.
        let device = match id {
            Some(id) => by_id(&host, Some(id)).ok_or("monitor output not found")?,
            // Resolved to the specific endpoint: a default-device stream
            // would follow the default by itself, e.g. onto the speakers.
            None => {
                let d = host.default_output_device().ok_or("no output device")?;
                let id = d.id().map_err(|e| describe_err(&e))?.to_string();
                by_id(&host, Some(&id)).ok_or("default output not found")?
            }
        };
        self.opened_id = device.id().ok().map(|id| id.to_string());
        self.device_name = device_name(&device, "output");
        let config = device
            .default_output_config()
            .map_err(|e| describe_err(&e))?;
        let mut sc: cpal::StreamConfig = config.config();
        if let cpal::SupportedBufferSize::Range { min, max } = *config.buffer_size() {
            sc.buffer_size = cpal::BufferSize::Fixed(OUT_BUFFER.clamp(min, max));
        }
        self.rearm();
        let (sh, tap) = (self.sh.clone(), self.tap.clone());
        let stream = super::by_format!(config.sample_format(), build(&device, sc, sh, tap))?;
        stream.play().map_err(|e| describe_err(&e))?;
        self.stream = Some(stream);
        self.tap.on.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Input device into output device in the same box, both its own
/// transducers (MacBook Microphone → MacBook Speakers).
pub fn risky_pair(input: &str, output: &str) -> bool {
    let (i, o) = (input.to_lowercase(), output.to_lowercase());
    i.contains("microphone") && o.contains("speaker")
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sh: Arc<Shared>,
    tap: Arc<MonitorTap>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = config.channels.max(1) as usize;
    let mut player = Player::new(config.sample_rate.max(1));
    let err_sh = sh.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| player.render(data, ch, &sh, &tap),
            move |err: cpal::Error| {
                // Flag only; the UI thread rebuilds (see `Monitor::tick`).
                if let ErrAction::Lost(msg) = classify(&err) {
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

/// Output-callback state: reads the tap a little behind the input's write
/// head, resampling input rate to output rate and nudging the speed so the
/// two clocks don't drift apart.
struct Player {
    out_rate: f32,
    /// Read position in the tap, absolute input frames.
    pos: Option<f64>,
    in_rate: u32,
    /// Smoothed distance behind the write head, input frames.
    delay: f64,
    /// Ran dry; wait for the full target delay before playing again, so
    /// it stays put instead of stuttering at the edge.
    starved: bool,
    fade: f32,
    /// 1 while armed, ramps to 0 once tripped.
    mute: f32,
    guard: Guard,
}

impl Player {
    fn new(out_rate: u32) -> Self {
        Self {
            out_rate: out_rate as f32,
            pos: None,
            in_rate: 0,
            delay: 0.0,
            starved: true,
            fade: 0.0,
            mute: 1.0,
            guard: Guard::new(out_rate as f32),
        }
    }

    fn render<T: SizedSample + FromSample<f32>>(
        &mut self,
        data: &mut [T],
        ch: usize,
        sh: &Shared,
        tap: &MonitorTap,
    ) {
        let silence = T::from_sample(0.0f32);
        if sh.rearm.swap(false, Ordering::Relaxed) {
            sh.trip.store(0, Ordering::Relaxed);
            self.guard = Guard::new(self.out_rate);
            self.fade = 0.0;
            self.mute = 1.0;
        }
        let in_rate = tap.rate.load(Ordering::Relaxed);
        if in_rate != self.in_rate {
            self.in_rate = in_rate;
            self.pos = None;
        }
        let frames = data.len() / ch;
        let ratio = in_rate as f64 / self.out_rate as f64;
        let need = frames as f64 * ratio + 2.0;
        let block = tap.block.load(Ordering::Relaxed).max(1) as f64;
        // Ride out an input callback landing just after we read, plus ours.
        let target = (block * 1.5 + need).max(in_rate as f64 * 0.004);
        let ceiling = (target * 3.0 + in_rate as f64 * 0.05).min(tap.ring.len() as f64 / 2.0);
        let w = tap.ring.written() as f64;
        let pos = *self.pos.get_or_insert((w - target).max(0.0));
        let behind = w - pos;
        if behind > ceiling {
            // Fell far behind (output stalled, or input resumed after a
            // pause): skip to fresh audio.
            self.pos = Some(w - target);
            self.delay = target;
        } else if behind < need || (self.starved && behind < target) {
            self.starved = true;
            data.fill(silence);
            return;
        }
        if std::mem::take(&mut self.starved) {
            self.delay = target;
        }
        if in_rate == 0 {
            data.fill(silence);
            return;
        }
        let mut pos = self.pos.unwrap_or(0.0);
        self.delay = self.delay * 0.95 + (w - pos) * 0.05;
        // A few tenths of a percent either way absorbs clock drift.
        let step = ratio * (1.0 + ((self.delay - target) / target).clamp(-1.0, 1.0) * 0.003);

        let gain = f32::from_bits(sh.gain.load(Ordering::Relaxed));
        let guard_on = sh.guard.load(Ordering::Relaxed);
        let sensitive = sh.sensitive.load(Ordering::Relaxed);
        let mut tripped = guard_on && sh.trip.load(Ordering::Relaxed) != 0;
        let fade_step = 1.0 / (FADE_IN_S * self.out_rate);
        let mute_step = 1.0 / (MUTE_S * self.out_rate);
        let mut pk = [0f32; 2];
        for frame in data.chunks_exact_mut(ch) {
            let i = pos.floor();
            let t = (pos - i) as f32;
            let ((l0, r0), (l1, r1)) = (tap.ring.get(i as usize), tap.ring.get(i as usize + 1));
            let (l, r) = (l0 + (l1 - l0) * t, r0 + (r1 - r0) * t);
            pos += step;

            if guard_on
                && !tripped
                && let Some(trip) = self.guard.feed(l, r, gain, sensitive)
            {
                sh.trip.store(trip.bits(), Ordering::Relaxed);
                tripped = true;
            }
            self.mute = if tripped {
                (self.mute - mute_step).max(0.0)
            } else {
                1.0
            };
            self.fade = (self.fade + fade_step).min(1.0);
            let g = gain * self.mute * self.fade * self.fade;
            let (l, r) = (soft_clip(l * g), soft_clip(r * g));
            pk = [pk[0].max(l.abs()), pk[1].max(r.abs())];
            match frame {
                [m] => *m = T::from_sample((l + r) * 0.5),
                [a, b, rest @ ..] => {
                    *a = T::from_sample(l);
                    *b = T::from_sample(r);
                    rest.fill(silence);
                }
                [] => {}
            }
        }
        self.pos = Some(pos);
        for (dst, p) in sh.peaks.iter().zip(pk) {
            dst.fetch_max(p.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Linear up to -1 dBFS, then bends into a hard ceiling at 0 dBFS, so a
/// runaway can't blast the headphones while the guard decides.
fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.89;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

const GUARD_BLOCK_S: f32 = 0.01;
/// Blocks per segment of the growth window (80 ms).
const SEG: usize = 8;
/// Segments in the growth window (240 ms).
const SEGS: usize = 3;

/// Acoustic feedback shows up as the input climbing on its own: close to a
/// pure tone, growing exponentially (a straight line in dB) until it clips.
/// Played material jumps up at the onset and then holds or decays (a kick,
/// a plucked bass, a tuning note). So, in 10 ms blocks of the monitored
/// signal, the guard looks for the level rising by about the same amount in
/// each of three 80 ms stretches in a row, kept up for a sizeable total rise,
/// and also trips on a tone held at clipping. All only while the output is
/// loud enough to drive a room.
struct Guard {
    block: usize,
    n: usize,
    sum: f32,
    peak: f32,
    /// Smoothed block levels (dBFS), newest at `at`.
    hist: [f32; SEGS * SEG + 1],
    at: usize,
    /// Blocks in a row loud enough to judge; the growth window only counts
    /// once it is all audible.
    live: usize,
    /// Level where the current steady climb started, if one is on.
    climb_from: Option<f32>,
    /// Smoothed peak / RMS: 1.41 for a sine, 3+ for most programme.
    crest: f32,
    /// Consecutive loud tonal blocks.
    hot: u32,
}

impl Guard {
    fn new(rate: f32) -> Self {
        Self {
            block: (rate * GUARD_BLOCK_S).max(1.0) as usize,
            n: 0,
            sum: 0.0,
            peak: 0.0,
            hist: [-120.0; SEGS * SEG + 1],
            at: 0,
            live: 0,
            climb_from: None,
            crest: 4.0,
            hot: 0,
        }
    }

    /// Smoothed level `k` blocks ago.
    fn ago(&self, k: usize) -> f32 {
        self.hist[(self.at + self.hist.len() - k) % self.hist.len()]
    }

    /// One monitored frame (before monitor gain). `Some` once it trips.
    fn feed(&mut self, l: f32, r: f32, gain: f32, sensitive: bool) -> Option<Trip> {
        use crate::ui::meter::to_db;
        self.sum += (l * l + r * r) * 0.5;
        self.peak = self.peak.max(l.abs().max(r.abs()));
        self.n += 1;
        if self.n < self.block {
            return None;
        }
        let rms = (self.sum / self.n as f32).sqrt();
        let peak = std::mem::take(&mut self.peak);
        (self.n, self.sum) = (0, 0.0);

        let crest = if rms > 1e-6 { peak / rms } else { 10.0 };
        self.crest = self.crest * 0.7 + crest * 0.3;
        let level = self.ago(0) * 0.6 + to_db(rms.max(1e-6)) * 0.4;
        self.at = (self.at + 1) % self.hist.len();
        self.hist[self.at] = level;

        // Too quiet, at the output, to be driving the room.
        if level + to_db(gain.max(1e-6)) < -45.0 || level < -70.0 {
            (self.climb_from, self.hot, self.live) = (None, 0, 0);
            return None;
        }
        self.live += 1;
        let (max_crest, min_seg, rise_db) = if sensitive {
            (3.0, 1.0, 8.0)
        } else {
            (2.2, 1.5, 12.0)
        };
        let tonal = self.crest < max_crest;
        const SPAN: usize = SEGS * SEG;
        let rises: [f32; SEGS] =
            std::array::from_fn(|k| self.ago(k * SEG) - self.ago((k + 1) * SEG));
        let (lo, hi) = rises
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &r| (lo.min(r), hi.max(r)));
        let steady = (0..SPAN).all(|k| self.ago(k) >= self.ago(k + 1) - 0.5);
        // Every stretch rising by about as much: a straight line in dB, not
        // an attack settling.
        let even = lo >= hi * 0.6;
        let judged = self.live > SPAN;
        if judged && tonal && steady && even && lo >= min_seg {
            let from = *self.climb_from.get_or_insert(self.ago(SPAN));
            if level - from >= rise_db {
                return Some(Trip::Runaway);
            }
        } else {
            self.climb_from = None;
        }
        // A tone held at clipping for 300 ms; next to its own speakers, a
        // loud tone held for a second.
        let held = if sensitive {
            peak > 0.3 && self.crest < 1.8
        } else {
            false
        };
        self.hot = if (peak > 0.95 && self.crest < 2.0) || held {
            self.hot + 1
        } else {
            0
        };
        let limit = if peak > 0.95 { 30 } else { 100 };
        (self.hot >= limit).then_some(Trip::Howl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(guard: &mut Guard, secs: f32, sensitive: bool, f: impl Fn(f32) -> f32) -> Option<Trip> {
        let rate = 48_000.0;
        (0..(secs * rate) as usize).find_map(|i| {
            let v = f(i as f32 / rate);
            guard.feed(v, v, 1.0, sensitive)
        })
    }

    #[test]
    fn growing_tone_trips() {
        let mut g = Guard::new(48_000.0);
        // 1 kHz, from -50 dB up 40 dB per second.
        let hit = run(&mut g, 1.5, false, |t| {
            (0.003 * 10f32.powf(2.0 * t)).min(1.0) * (t * 1000.0 * std::f32::consts::TAU).sin()
        });
        assert_eq!(hit, Some(Trip::Runaway));
    }

    #[test]
    fn repeating_kicks_and_steady_tone_pass() {
        let mut g = Guard::new(48_000.0);
        // A decaying 50 Hz kick every 500 ms.
        let kick = |t: f32| {
            let u = t % 0.5;
            0.9 * (-u * 12.0).exp() * (u * 50.0 * std::f32::consts::TAU).sin()
        };
        assert_eq!(run(&mut g, 4.0, false, kick), None);
        // A steady tuning note, starting from silence.
        let mut g = Guard::new(48_000.0);
        assert_eq!(
            run(&mut g, 4.0, false, |t| 0.5
                * (t * 110.0 * std::f32::consts::TAU).sin()),
            None
        );
        // A quieter one even next to the speakers.
        let mut g = Guard::new(48_000.0);
        assert_eq!(
            run(&mut g, 4.0, true, |t| 0.2
                * (t * 110.0 * std::f32::consts::TAU).sin()),
            None
        );
        // A sine pad with a slow 400 ms linear attack, every second.
        let mut g = Guard::new(48_000.0);
        let pad =
            |t: f32| 0.8 * ((t % 1.0) / 0.4).min(1.0) * (t * 220.0 * std::f32::consts::TAU).sin();
        assert_eq!(run(&mut g, 4.0, false, pad), None);
    }

    #[test]
    fn clipped_tone_trips() {
        let mut g = Guard::new(48_000.0);
        let hit = run(&mut g, 1.0, false, |t| {
            (3.0 * (t * 800.0 * std::f32::consts::TAU).sin()).clamp(-1.0, 1.0)
        });
        assert_eq!(hit, Some(Trip::Howl));
    }
}

//! The front end shared by the note-range analysers (tuner, spectrum):
//! decide whether there is new audio worth analysing, then low-pass and
//! decimate the analysis window so the per-note work runs on a few thousand
//! samples per second instead of 48k. Plus the per-note DFT they both use.

use std::sync::Arc;

use super::Shared;
use super::perf::Costs;
use crate::pitch;
use crate::ui::noterange::NoteRange;

/// Level reported for "nothing here" (too short a window, silence).
pub const FLOOR_DB: f32 = -140.0;
/// Longest decimation filter span, in input samples.
const MAX_TAPS: usize = 32 * 28 + 1;

/// Low-pass + downsample by a power of two.
pub struct Decimator {
    rate: u32,
    factor: usize,
    taps: Vec<f32>,
}

impl Decimator {
    /// Largest power-of-two factor (≤ 32) that keeps the new rate at least
    /// `oversample` times the top frequency, so NSDF lags stay well resolved.
    pub fn factor_for(rate: u32, f_top: f32, oversample: f32) -> usize {
        let mut factor = 1;
        while factor < 32 && rate as f32 / (2 * factor) as f32 >= oversample * f_top {
            factor *= 2;
        }
        factor
    }

    pub fn new(rate: u32, factor: usize) -> Self {
        // Blackman windowed sinc, cutoff at the new Nyquist, transition
        // about 0.2 of the new rate: content that aliases lands above 0.4.
        let len = if factor == 1 { 1 } else { 28 * factor + 1 };
        let mid = (len / 2) as f32;
        let fc = 0.5 / factor as f32;
        let mut taps: Vec<f32> = (0..len)
            .map(|i| {
                if len == 1 {
                    return 1.0;
                }
                let t = i as f32 - mid;
                let sinc = if t == 0.0 {
                    2.0 * fc
                } else {
                    (std::f32::consts::TAU * fc * t).sin() / (std::f32::consts::PI * t)
                };
                let u = i as f32 / (len - 1) as f32;
                let w = 0.42 - 0.5 * (std::f32::consts::TAU * u).cos()
                    + 0.08 * (2.0 * std::f32::consts::TAU * u).cos();
                sinc * w
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self { rate, factor, taps }
    }

    pub fn out_rate(&self) -> u32 {
        self.rate / self.factor as u32
    }

    /// Filtered, decimated `x`, aligned to its newest fully-supported sample.
    pub fn run(&self, x: &[f32]) -> Vec<f32> {
        if self.factor == 1 {
            return x.to_vec();
        }
        let len = self.taps.len();
        if x.len() < len {
            return Vec::new();
        }
        let last = x.len() - len; // start of the newest full filter span
        let count = last / self.factor + 1;
        let first = last - (count - 1) * self.factor;
        (0..count)
            .map(|j| {
                let s = first + j * self.factor;
                x[s..s + len]
                    .iter()
                    .zip(&self.taps)
                    .map(|(a, b)| a * b)
                    .sum()
            })
            .collect()
    }
}

/// How a module wants its input prepared.
pub struct Want {
    pub window_ms: f32,
    /// Free-running analyses per second.
    pub rate_hz: f32,
    /// Decimated rate vs the top note.
    pub oversample: f32,
    pub a4: f32,
    /// Ceiling (fractional MIDI) on the analysed band, whatever the shared
    /// range shows. The decimated rate, and so the analysis cost, follows it.
    pub max_midi: f32,
}

impl Want {
    /// `midi` held to the analysis ceiling.
    pub fn top(&self, midi: f32) -> f32 {
        midi.min(self.max_midi)
    }

    /// Samples of input needed at `rate`: the window plus the filter span.
    pub fn lookback(&self, rate: u32) -> usize {
        (self.window_ms / 1000.0 * rate as f32) as usize + MAX_TAPS
    }
}

/// One prepared analysis window.
pub struct Window {
    /// Decimated samples, newest last.
    pub y: Vec<f32>,
    pub rate: u32,
    /// RMS level of `y`, dB.
    pub level_db: f32,
}

/// Raw audio cut for one analysis, ready to decimate on any thread.
pub struct Cut {
    x: Vec<f32>,
    decim: Arc<Decimator>,
    window_ms: f32,
}

impl Cut {
    /// Low-pass, decimate and trim to the window.
    pub fn run(&self) -> Window {
        let d = &self.decim;
        let y = d.run(&self.x);
        let r = d.out_rate();
        let keep = (self.window_ms / 1000.0 * r as f32) as usize;
        let y = y[y.len().saturating_sub(keep)..].to_vec();
        let rms = (y.iter().map(|s| s * s).sum::<f32>() / y.len().max(1) as f32).sqrt();
        Window {
            y,
            rate: r,
            level_db: 20.0 * rms.max(1e-9).log10(),
        }
    }
}

/// Pacing and decimation state for one module.
#[derive(Default)]
pub struct NoteInput {
    decim: Option<Arc<Decimator>>,
    /// What was analysed last: the gate window, or the stream position in
    /// free mode. Unchanged input is not analysed again.
    last: Option<(u64, u64)>,
    /// The note range the latest analysis covers.
    pub analysed: NoteRange,
}

impl NoteInput {
    /// The next window to analyse, or `None` when nothing new arrived (or
    /// the free-running hop hasn't elapsed). Stage 0 of `costs` is decimation.
    pub fn next(&mut self, shared: &Shared, want: &Want, costs: &mut Costs) -> Option<Window> {
        let cut = self.prepare(shared, want)?;
        costs.tick();
        Some(costs.time(0, || cut.run()))
    }

    /// The raw audio for the next analysis (see [`NoteInput::next`]), cut
    /// but not decimated, so the heavy part can run off the UI thread.
    pub fn prepare(&mut self, shared: &Shared, want: &Want) -> Option<Cut> {
        let f = &shared.frame;
        let rate = f.rate;
        let key = match shared.gate.window {
            Some(w) => w,
            None => {
                let end = f.end();
                let hop = f.ms_to_samples(1000.0 / want.rate_hz) as u64;
                match self.last {
                    Some((0, t)) if !shared.reset && end >= t && end - t < hop => return None,
                    _ => (0, end),
                }
            }
        };
        let range = shared.range.get();
        if self.last == Some(key) && !shared.reset && range == self.analysed {
            return None;
        }
        self.last = Some(key);
        self.analysed = range;

        let f_hi = pitch::midi_to_freq(want.top(range.span().1 + 0.5), want.a4);
        let factor = Decimator::factor_for(rate, f_hi, want.oversample);
        if !self
            .decim
            .as_ref()
            .is_some_and(|d| d.rate == rate && d.factor == factor)
        {
            self.decim = Some(Arc::new(Decimator::new(rate, factor)));
        }
        let decim = self.decim.clone().unwrap();
        let window = (want.window_ms / 1000.0 * rate as f32) as usize;
        let x = shared.analysis_slice(want.lookback(rate));
        let x = x[x.len().saturating_sub(window + decim.taps.len())..].to_vec();
        Some(Cut {
            x,
            decim,
            window_ms: want.window_ms,
        })
    }
}

/// Hann-windowed DFT magnitude (dB re full-scale sine) at `freq`, over the
/// newest `max_cycles` periods of `y` (or all of it, if shorter).
pub fn note_db(y: &[f32], rate: u32, freq: f32, max_cycles: f32) -> f32 {
    let f = freq as f64;
    let l = ((max_cycles as f64 * rate as f64 / f) as usize).min(y.len());
    if l < 16 {
        return FLOOR_DB;
    }
    let x = &y[y.len() - l..];
    // Phasor and window by rotation, no trig per sample.
    let w = std::f64::consts::TAU * f / rate as f64;
    let (wc, ws) = (w.cos(), w.sin());
    let h = std::f64::consts::TAU / l as f64;
    let (hc, hs) = (h.cos(), h.sin());
    let (mut pr, mut pi) = (1.0f64, 0.0f64);
    let (mut qr, mut qi) = (1.0f64, 0.0f64);
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for &s in x {
        let v = s as f64 * (0.5 - 0.5 * qr);
        re += v * pr;
        im -= v * pi;
        (pr, pi) = (pr * wc - pi * ws, pr * ws + pi * wc);
        (qr, qi) = (qr * hc - qi * hs, qr * hs + qi * hc);
    }
    // A sine of amplitude A peaks at A·L/4 through a Hann window.
    let amp = (re * re + im * im).sqrt() * 4.0 / l as f64;
    (20.0 * amp.max(1e-9).log10()) as f32
}

/// Exponential approach for a frame of `dt` seconds with time constant `tau`.
pub fn ease(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt / tau.max(1e-4)).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decimated low E with harmonics still lands on 41.2 Hz.
    #[test]
    fn decimated_low_e() {
        let rate = 48_000;
        let x: Vec<f32> = (0..rate as usize / 3)
            .map(|i| {
                let t = std::f32::consts::TAU * 41.2 * i as f32 / rate as f32;
                0.3 * t.sin() + 0.4 * (2.0 * t).sin() + 0.2 * (3.0 * t).sin()
            })
            .collect();
        let d = Decimator::new(rate, Decimator::factor_for(rate, 300.0, 10.0));
        assert!(d.factor > 1);
        let y = d.run(&x);
        let f = pitch::candidates(&y, d.out_rate(), 30.0, 300.0)
            .pick()
            .unwrap();
        assert!((f - 41.2).abs() < 0.05, "{f}");
    }
}

//! Short-time spectra for the waterfall: a small radix-2 FFT, a Hann window,
//! and a warped frequency axis that gives the lows room without letting a
//! pure log scale spend half the width below 200 Hz.

use std::f32::consts::TAU;

/// Lowest frequency shown.
pub const F_MIN: f32 = 20.0;
/// Knee of the warp: roughly linear below, logarithmic above. Smaller means
/// more log-like (more room for the bass).
const F_KNEE: f32 = 180.0;
/// Display tilt, dB per octave around 1 kHz, so typical program material
/// (which falls off with frequency) looks roughly level.
const TILT_DB_PER_OCT: f32 = 3.0;
/// Below this, a bin reads as silence.
pub const FLOOR_DB: f32 = -96.0;

fn warp(f: f32) -> f32 {
    (1.0 + f / F_KNEE).ln()
}

fn unwarp(w: f32) -> f32 {
    F_KNEE * (w.exp() - 1.0)
}

/// Where `freq` sits across the axis, 0..1, for a top frequency of `f_max`.
pub fn axis_pos(freq: f32, f_max: f32) -> f32 {
    let (a, b) = (warp(F_MIN), warp(f_max));
    ((warp(freq) - a) / (b - a)).clamp(0.0, 1.0)
}

/// The frequency at axis position `u` (0..1).
pub fn axis_freq(u: f32, f_max: f32) -> f32 {
    let (a, b) = (warp(F_MIN), warp(f_max));
    unwarp(a + u * (b - a))
}

/// A reusable radix-2 FFT plan of one size with a Hann window. Plans are
/// kept across frames (see `modules::Shared`); spectra are not.
pub struct Fft {
    n: usize,
    window: Vec<f32>,
    twiddle: Vec<(f32, f32)>,
    rev: Vec<usize>,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Fft {
    /// `n` must be a power of two.
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 16);
        let window = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()).collect();
        let twiddle = (0..n / 2)
            .map(|k| {
                let a = -TAU * k as f32 / n as f32;
                (a.cos(), a.sin())
            })
            .collect();
        let bits = n.trailing_zeros();
        let rev = (0..n).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        Self { n, window, twiddle, rev, re: vec![0.0; n], im: vec![0.0; n] }
    }

    /// Windowed power spectrum of `x` (exactly `n` samples): `n/2 + 1` bins,
    /// scaled so a full-scale sine peaks at about 1.0 (0 dB).
    pub fn power(&mut self, x: &[f32]) -> Vec<f32> {
        debug_assert_eq!(x.len(), self.n);
        for (i, &s) in x.iter().enumerate() {
            let r = self.rev[i];
            self.re[r] = s * self.window[i];
            self.im[r] = 0.0;
        }
        self.fft();
        // Hann window: a sine of amplitude A peaks at A·n/4.
        let scale = 4.0 / self.n as f32;
        (0..=self.n / 2)
            .map(|k| {
                let (re, im) = (self.re[k] * scale, self.im[k] * scale);
                re * re + im * im
            })
            .collect()
    }

    /// In-place iterative radix-2 FFT over `re`/`im` (already bit-reversed).
    fn fft(&mut self) {
        let n = self.n;
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let stride = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..half {
                    let (wr, wi) = self.twiddle[k * stride];
                    let (a, b) = (start + k, start + k + half);
                    let (xr, xi) = (self.re[b], self.im[b]);
                    let tr = xr * wr - xi * wi;
                    let ti = xr * wi + xi * wr;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            len *= 2;
        }
    }
}

/// Maps an FFT power spectrum onto display bands along the warped axis,
/// with the display tilt applied.
pub struct Bands {
    n: usize,
    rate: u32,
    /// Per output band: FFT bin range `[lo, hi)`, or a fractional bin to
    /// interpolate at when the band is narrower than one bin.
    bands: Vec<Band>,
    /// Per band dB offset for the display tilt.
    tilt: Vec<f32>,
}

enum Band {
    Range(usize, usize),
    Lerp(f32),
}

impl Bands {
    /// Layout for an `n`-point FFT at `rate`, `count` bands wide.
    pub fn new(n: usize, rate: u32, count: usize) -> Self {
        let f_max = rate as f32 / 2.0;
        let hz_per_bin = rate as f32 / n as f32;
        let mut bands = Vec::with_capacity(count);
        let mut tilt = Vec::with_capacity(count);
        for j in 0..count {
            let lo = axis_freq(j as f32 / count as f32, f_max);
            let hi = axis_freq((j + 1) as f32 / count as f32, f_max);
            let (blo, bhi) = ((lo / hz_per_bin).round() as usize, (hi / hz_per_bin).round() as usize);
            bands.push(if bhi > blo { Band::Range(blo, bhi.min(n / 2)) } else { Band::Lerp(((lo + hi) / 2.0) / hz_per_bin) });
            let centre = ((lo * hi).sqrt()).max(1.0);
            tilt.push(TILT_DB_PER_OCT * (centre / 1000.0).log2());
        }
        Self { n, rate, bands, tilt }
    }

    pub fn fits(&self, n: usize, rate: u32) -> bool {
        self.n == n && self.rate == rate
    }

    /// dB per band (peak within each band) for a spectrum from `Fft::power`.
    pub fn db(&self, power: &[f32]) -> Vec<f32> {
        let last = power.len().saturating_sub(1);
        let p = |k: usize| power.get(k.min(last)).copied().unwrap_or(0.0);
        self.bands
            .iter()
            .zip(&self.tilt)
            .map(|(b, t)| {
                let v = match *b {
                    Band::Range(lo, hi) => (lo..hi).map(p).fold(0f32, f32::max),
                    Band::Lerp(x) => {
                        let k = x.floor() as usize;
                        let f = x - k as f32;
                        p(k) * (1.0 - f) + p(k + 1) * f
                    }
                };
                (10.0 * v.max(1e-12).log10() + t).max(FLOOR_DB)
            })
            .collect()
    }
}

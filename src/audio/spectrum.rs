//! Short-time spectra for the waterfall and Impulse: a small radix-2 FFT, a Hann window,
//! and log-spaced display bands (the shared note axis is log frequency).

use std::f32::consts::{PI, TAU};

/// Display tilt, dB per octave around 1 kHz, so typical program material
/// (which falls off with frequency) looks roughly level.
pub const TILT_DB_PER_OCT: f32 = 3.0;
/// Below this, a bin reads as silence.
pub const FLOOR_DB: f32 = -96.0;

/// A reusable real-input FFT plan of one size with a Hann window. Plans are
/// kept across frames (see `modules::Shared`); spectra are not.
///
/// A real `n`-point transform is done as one `n/2`-point complex FFT (even
/// samples in the real part, odd in the imaginary part) and then unpacked,
/// which costs about half of a complex FFT of the zero-padded input.
pub struct Fft {
    n: usize,
    /// Hann window, `n` long.
    window: Vec<f32>,
    /// Bit-reversal permutation of the `n/2`-point complex FFT.
    rev: Vec<usize>,
    /// Butterfly twiddles, one contiguous run per stage: the stage with
    /// half-size `h` uses `stages[h-1 .. 2h-1]`.
    stages: Vec<(f32, f32)>,
    /// `exp(-2πik/n)` for `k` in `0..=n/2`, for unpacking the real spectrum.
    post: Vec<(f32, f32)>,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Fft {
    /// `n` must be a power of two.
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 16);
        let m = n / 2;
        let window = (0..n)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos())
            .collect();
        let bits = m.trailing_zeros();
        let rev = (0..m)
            .map(|i| i.reverse_bits() >> (usize::BITS - bits))
            .collect();
        let mut stages = vec![(0.0, 0.0); m - 1];
        let mut h = 1;
        while h < m {
            for k in 0..h {
                let a = -PI * k as f32 / h as f32;
                stages[h - 1 + k] = (a.cos(), a.sin());
            }
            h *= 2;
        }
        let post = (0..=m)
            .map(|k| {
                let a = -TAU * k as f32 / n as f32;
                (a.cos(), a.sin())
            })
            .collect();
        Self {
            n,
            window,
            rev,
            stages,
            post,
            re: vec![0.0; m],
            im: vec![0.0; m],
        }
    }

    /// Windowed power spectrum of `x` (exactly `n` samples): `n/2 + 1` bins,
    /// scaled so a full-scale sine peaks at about 1.0 (0 dB).
    pub fn power(&mut self, x: &[f32]) -> Vec<f32> {
        debug_assert_eq!(x.len(), self.n);
        let m = self.n / 2;
        for (j, (s, w)) in x
            .chunks_exact(2)
            .zip(self.window.chunks_exact(2))
            .enumerate()
        {
            let r = self.rev[j];
            self.re[r] = s[0] * w[0];
            self.im[r] = s[1] * w[1];
        }
        self.fft();
        // Hann window: a sine of amplitude A peaks at A·n/4.
        let scale = 4.0 / self.n as f32;
        (0..=m)
            .map(|k| {
                // Z = E + iO holds the even and odd halves' spectra;
                // X[k] = E[k] + W^k·O[k].
                let (zr, zi) = (self.re[k % m], self.im[k % m]);
                let (cr, ci) = (self.re[(m - k) % m], -self.im[(m - k) % m]);
                let (er, ei) = (0.5 * (zr + cr), 0.5 * (zi + ci));
                let (dr, di) = (zr - cr, zi - ci);
                let (or, oi) = (0.5 * di, -0.5 * dr);
                let (wr, wi) = self.post[k];
                let xr = (er + wr * or - wi * oi) * scale;
                let xi = (ei + wr * oi + wi * or) * scale;
                xr * xr + xi * xi
            })
            .collect()
    }

    /// In-place iterative radix-2 FFT over `re`/`im` (already bit-reversed),
    /// `n/2` points.
    fn fft(&mut self) {
        let m = self.n / 2;
        let mut h = 1;
        while h < m {
            let len = 2 * h;
            let tw = &self.stages[h - 1..2 * h - 1];
            for (re, im) in self
                .re
                .chunks_exact_mut(len)
                .zip(self.im.chunks_exact_mut(len))
            {
                let (ra, rb) = re.split_at_mut(h);
                let (ia, ib) = im.split_at_mut(h);
                let halves = ra.iter_mut().zip(ia.iter_mut());
                let others = rb.iter_mut().zip(ib.iter_mut());
                for (((ar, ai), (br, bi)), &(wr, wi)) in halves.zip(others).zip(tw) {
                    let (xr, xi) = (*br, *bi);
                    let tr = xr * wr - xi * wi;
                    let ti = xr * wi + xi * wr;
                    *br = *ar - tr;
                    *bi = *ai - ti;
                    *ar += tr;
                    *ai += ti;
                }
            }
            h = len;
        }
    }
}

/// Maps an FFT power spectrum onto log-spaced display bands, with the
/// display tilt applied.
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
    /// Layout for an `n`-point FFT at `rate`, `count` bands evenly spaced
    /// in log frequency across `(f_lo, f_hi)` Hz.
    pub fn new(n: usize, rate: u32, count: usize, (f_lo, f_hi): (f32, f32)) -> Self {
        let axis_freq = |u: f32| f_lo * (f_hi / f_lo).powf(u);
        let hz_per_bin = rate as f32 / n as f32;
        let mut bands = Vec::with_capacity(count);
        let mut tilt = Vec::with_capacity(count);
        for j in 0..count {
            let lo = axis_freq(j as f32 / count as f32);
            let hi = axis_freq((j + 1) as f32 / count as f32);
            let (blo, bhi) = (
                (lo / hz_per_bin).round() as usize,
                (hi / hz_per_bin).round() as usize,
            );
            bands.push(if bhi > blo {
                Band::Range(blo, bhi.min(n / 2))
            } else {
                Band::Lerp(((lo + hi) / 2.0) / hz_per_bin)
            });
            let centre = ((lo * hi).sqrt()).max(1.0);
            tilt.push(TILT_DB_PER_OCT * (centre / 1000.0).log2());
        }
        Self {
            n,
            rate,
            bands,
            tilt,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The packed real FFT must match a direct DFT of the windowed input.
    #[test]
    fn matches_direct_dft() {
        for n in [16usize, 64, 256] {
            let x: Vec<f32> = (0..n)
                .map(|i| ((i * 37 + 11) % 97) as f32 / 97.0 - 0.5)
                .collect();
            let got = Fft::new(n).power(&x);
            let scale = 4.0 / n as f64;
            let want: Vec<f64> = (0..=n / 2)
                .map(|k| {
                    let (mut re, mut im) = (0.0f64, 0.0f64);
                    for (i, &s) in x.iter().enumerate() {
                        let w = (0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()) as f64;
                        let a = -(TAU as f64) * (i * k) as f64 / n as f64;
                        re += s as f64 * w * a.cos();
                        im += s as f64 * w * a.sin();
                    }
                    (re * re + im * im) * scale * scale
                })
                .collect();
            let peak = want.iter().copied().fold(0.0, f64::max);
            for (k, (&g, &w)) in got.iter().zip(&want).enumerate() {
                assert!(
                    (g as f64 - w).abs() <= 1e-4 * peak + 1e-12,
                    "n={n} k={k}: {g} vs {w}"
                );
            }
        }
    }
}

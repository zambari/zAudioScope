//! What the shared note range leaves out: the spectrum below the selection
//! (down to the bar's lowest note) and above it (up to Nyquist), drawn into
//! the dimmed parts of the range bar (see [`super::noterange::bar`]).
//!
//! It reuses the largest FFT some analyser already computed this frame
//! (`Shared::any_spectrum`), so it is nearly free when the spectrogram or
//! waterfall is on; otherwise it runs one small FFT of its own.

use egui::{Align2, Color32, FontId, Painter, Rect, Stroke, pos2};

use super::noterange::NoteRange;
use super::plot::{self, Scale};
use super::theme;
use crate::audio::spectrum::{Bands, FLOOR_DB};

/// Display bands per side.
const BANDS: usize = 40;
/// Per-frame smoothing of bands and stats (1 = none).
const SMOOTH: f32 = 0.25;
/// A Hann-windowed sine spreads its power over ~1.5 bins' worth, so the
/// summed level of a lone full-scale sine reads 0 dB.
const HANN_SUM: f32 = 1.5;
/// Band dB mapped to the strip's height.
const DB_SPAN: (f32, f32) = (-84.0, -6.0);

/// One side of the selection.
#[derive(Clone, Default)]
pub struct Side {
    /// Frequency span covered, Hz.
    pub hz: (f32, f32),
    /// Smoothed peak dB per log-spaced band (display tilt applied).
    pub db: Vec<f32>,
    /// Total power in the span, dB (a full-scale sine ≈ 0).
    pub level_db: f32,
    /// Power-weighted geometric mean frequency, Hz.
    pub centroid: f32,
    /// Strongest bin, Hz (not smoothed).
    pub peak: f32,
    /// Spectral flatness 0 (pure tone) .. 1 (white noise).
    pub flatness: f32,
    /// FFT bins inside the span.
    pub bins: usize,
}

#[derive(Clone, Default)]
pub struct Outside {
    pub lo: Side,
    pub hi: Side,
    /// FFT size the bins came from, and whether another analyser paid for it.
    pub fft: usize,
    pub reused: bool,
}

impl Outside {
    pub fn update(&mut self, power: &[f32], n: usize, rate: u32, range: NoteRange, reused: bool) {
        let (s0, s1) = range.hz();
        self.fft = n;
        self.reused = reused;
        self.lo.update(power, n, rate, (NoteRange::BOUNDS.hz().0, s0));
        self.hi.update(power, n, rate, (s1, rate as f32 / 2.0));
    }

    /// Hover text for whichever side `low` picks.
    pub fn describe(&self, low: bool) -> String {
        let s = if low { &self.lo } else { &self.hi };
        let tone = if s.bins >= 4 {
            format!("{:.0}% tonal", (1.0 - s.flatness) * 100.0)
        } else {
            "too few bins to judge".to_owned()
        };
        format!(
            "{} the range: {}–{} Hz\nlevel {:.1} dB · peak {} Hz · centroid {} Hz\n{tone} · {} bins of a {}-pt FFT ({})",
            if low { "Below" } else { "Above" },
            plot::fmt_hz(s.hz.0.round()),
            plot::fmt_hz(s.hz.1.round()),
            s.level_db,
            plot::fmt_hz(s.peak.round()),
            plot::fmt_hz(s.centroid.round()),
            s.bins,
            self.fft,
            if self.reused { "shared" } else { "own" },
        )
    }

    /// Draw both sides: `lo` / `hi` are the dimmed key areas left and right
    /// of the selection. The low side lines up with the keys; the high side
    /// squeezes everything up to Nyquist into its area.
    pub fn paint(&self, p: &Painter, lo: Rect, hi: Rect) {
        self.lo.paint(p, lo, false);
        self.hi.paint(p, hi, true);
    }
}

impl Side {
    fn update(&mut self, power: &[f32], n: usize, rate: u32, hz: (f32, f32)) {
        self.hz = hz;
        let span = (hz.0, hz.1.max(hz.0 * 1.001));
        let db = Bands::new(n, rate, BANDS, span).db(power);
        let fresh = self.db.len() != db.len();
        let a = if fresh { 1.0 } else { SMOOTH };
        let ema = |old: &mut f32, new: f32| *old += (new - *old) * a;
        if fresh {
            self.db = db;
        } else {
            self.db.iter_mut().zip(db).for_each(|(o, v)| ema(o, v));
        }

        // Stats from the raw bins inside the span.
        let hz_per_bin = rate as f32 / n as f32;
        let k0 = ((span.0 / hz_per_bin).ceil() as usize).max(1);
        let k1 = ((span.1 / hz_per_bin).floor() as usize).min(power.len().saturating_sub(1));
        let (mut sum, mut sum_lnf, mut sum_lnp, mut best) = (0f64, 0f64, 0f64, (0f32, 0usize));
        for (k, &p) in power.iter().enumerate().take(k1 + 1).skip(k0) {
            let f = k as f32 * hz_per_bin;
            sum += p as f64;
            sum_lnf += p as f64 * (f as f64).ln();
            sum_lnp += (p.max(1e-12) as f64).ln();
            if p > best.0 {
                best = (p, k);
            }
        }
        self.bins = (k1 + 1).saturating_sub(k0);
        let level = (10.0 * (sum as f32 / HANN_SUM).max(1e-12).log10()).max(FLOOR_DB);
        ema(&mut self.level_db, level);
        if sum > 1e-12 {
            let c = (sum_lnf / sum).exp() as f32;
            self.centroid = if fresh || self.centroid <= 0.0 { c } else { (self.centroid.ln() + (c.ln() - self.centroid.ln()) * a).exp() };
            self.peak = best.1 as f32 * hz_per_bin;
            let mean = sum / self.bins as f64;
            let flat = ((sum_lnp / self.bins as f64).exp() / mean) as f32;
            ema(&mut self.flatness, flat.clamp(0.0, 1.0));
        }
    }

    fn paint(&self, p: &Painter, rect: Rect, high: bool) {
        if rect.width() < 3.0 || self.db.is_empty() {
            return;
        }
        // Columns: tonal content in the accent colour, noise in grey.
        let tone = Color32::from_gray(150).lerp_to_gamma(theme::ACCENT_BRIGHT, 1.0 - self.flatness);
        let fill = tone.gamma_multiply(0.55);
        let w = rect.width() / self.db.len() as f32;
        let y = Scale::linear(DB_SPAN, (rect.bottom(), rect.top()));
        for (j, &db) in self.db.iter().enumerate() {
            let top = y.px(db).clamp(rect.top(), rect.bottom());
            if top < rect.bottom() - 0.5 {
                let x0 = rect.left() + j as f32 * w;
                p.rect_filled(Rect::from_min_max(pos2(x0, top), pos2(x0 + w.max(1.0), rect.bottom())), 0.0, fill);
            }
        }
        // Strongest bin and the centroid.
        let x = Scale::log(self.hz, (rect.left(), rect.right()));
        if self.level_db > FLOOR_DB + 1.0 {
            p.vline(x.px(self.peak), rect.y_range(), Stroke::new(1.0, theme::ON_ACCENT.gamma_multiply(0.7)));
            let c = x.px(self.centroid).clamp(rect.left(), rect.right());
            p.circle_filled(pos2(c, rect.bottom() - 3.0), 2.0, Color32::WHITE);
        }
        // Total level as a thin meter on the outer edge.
        let m = Scale::linear((DB_SPAN.0, 0.0), (rect.bottom(), rect.top()));
        let mx = if high { rect.right() - 3.0 } else { rect.left() };
        let mt = m.px(self.level_db).clamp(rect.top(), rect.bottom());
        p.rect_filled(Rect::from_min_max(pos2(mx, mt), pos2(mx + 3.0, rect.bottom())), 0.0, tone);
        if rect.width() > 70.0 {
            let (at, align) = if high {
                (pos2(rect.right() - 5.0, rect.top() + 1.0), Align2::RIGHT_TOP)
            } else {
                (pos2(rect.left() + 5.0, rect.top() + 1.0), Align2::LEFT_TOP)
            };
            p.text(
                at,
                align,
                format!("{:.0} dB · {}", self.level_db, plot::fmt_hz(self.peak.round())),
                FontId::monospace(9.0),
                Color32::from_gray(200),
            );
        }
    }
}

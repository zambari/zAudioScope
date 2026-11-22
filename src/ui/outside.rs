//! What the shared note range leaves out: the spectrum below the selection
//! (down to the bar's lowest note) and above it (up to Nyquist), drawn into
//! the dimmed parts of the range bar (see [`super::noterange::bar`]).
//!
//! Bands sit on a fixed quarter-tone grid in absolute pitch, so moving the
//! selection only changes which bands count as "outside", never where they
//! are. Each band reads from the finest spectrum that covers it: a 16×
//! decimated FFT (sub-Hz bins) for the low end, and for the rest the largest
//! FFT an analyser already ran this frame (`Shared::any_spectrum`) or one of
//! our own.

use egui::{Align2, Color32, FontId, Painter, Rect, Stroke, pos2};

use super::noterange::{A4, NoteRange};
use super::plot::{self, Scale};
use super::theme;
use crate::audio::spectrum::{FLOOR_DB, TILT_DB_PER_OCT};
use crate::pitch;

/// Bands per semitone.
const BPS: f32 = 4.0;
/// Fine source: decimation factor and FFT size (0.73 Hz bins at 48 kHz).
pub const FINE_DECIMATE: u32 = 16;
pub const FINE_FFT: usize = 4096;
/// Coarse source size when no analyser computed a spectrum this frame.
pub const COARSE_FFT: usize = 4096;
/// The decimator is gentle; trust the fine source only this far up its band.
pub const FINE_TOP: f32 = 0.35;
/// Width of the always-visible "air" zone (bar top .. Nyquist), px.
pub const AIR_W: f32 = 56.0;
/// Per-frame smoothing of bands and stats (1 = none).
const SMOOTH: f32 = 0.25;
/// A Hann-windowed sine spreads its power over ~1.5 bins' worth, so the
/// summed level of a lone full-scale sine reads 0 dB.
const HANN_SUM: f32 = 1.5;
/// Band dB mapped to the strip's height.
const DB_SPAN: (f32, f32) = (-84.0, -6.0);

/// One power spectrum and how to read it.
pub struct Source<'a> {
    pub power: &'a [f32],
    /// Bin width, Hz.
    pub hz_per_bin: f32,
    /// Highest frequency this source is trusted for, Hz.
    pub top: f32,
}

/// One band's reading: peak power, summed power, and the (fractional,
/// parabola-refined) frequency of its strongest bin.
struct Reading {
    peak: f32,
    sum: f32,
    peak_hz: f32,
}

impl Source<'_> {
    pub fn new(power: &[f32], rate: f32, n: usize, top: f32) -> Source<'_> {
        Source { power, hz_per_bin: rate / n as f32, top }
    }

    fn p(&self, k: usize) -> f32 {
        self.power.get(k.min(self.power.len().saturating_sub(1))).copied().unwrap_or(0.0)
    }

    fn lerp(&self, x: f32) -> f32 {
        let k = x.floor().max(0.0) as usize;
        let t = x - k as f32;
        self.p(k) * (1.0 - t) + self.p(k + 1) * t
    }

    fn band(&self, (f0, f1): (f32, f32)) -> Reading {
        let (b0, b1) = (f0 / self.hz_per_bin, f1 / self.hz_per_bin);
        let (k0, k1) = (b0.round() as usize, (b1.round() as usize).min(self.power.len()));
        if k1 > k0 {
            let (mut sum, mut best) = (0.0, k0);
            for k in k0..k1 {
                sum += self.power[k];
                if self.power[k] > self.power[best] {
                    best = k;
                }
            }
            Reading { peak: self.power[best], sum, peak_hz: self.refine(best) * self.hz_per_bin }
        } else {
            // Narrower than a bin: interpolate, and take its share of the bin.
            let c = (b0 + b1) / 2.0;
            let v = self.lerp(c);
            Reading { peak: v, sum: v * (b1 - b0), peak_hz: c * self.hz_per_bin }
        }
    }

    /// Fractional bin of the peak at `k`, by a parabola through log power.
    fn refine(&self, k: usize) -> f32 {
        if k == 0 || k + 1 >= self.power.len() {
            return k as f32;
        }
        let l = |k: usize| self.power[k].max(1e-20).ln();
        let (a, b, c) = (l(k - 1), l(k), l(k + 1));
        let d = a - 2.0 * b + c;
        if d < 0.0 { k as f32 + (0.5 * (a - c) / d).clamp(-0.5, 0.5) } else { k as f32 }
    }
}

/// Summary of one side of the selection.
#[derive(Clone, Default)]
pub struct Side {
    /// Pitch span covered, fractional MIDI.
    pub midi: (f32, f32),
    /// Total power in the span, dB (a full-scale sine ≈ 0).
    pub level_db: f32,
    /// Power-weighted geometric mean frequency, Hz.
    pub centroid: f32,
    /// Strongest component, Hz (refined between bins).
    pub peak: f32,
    /// Flatness of the per-band power, 0 (one tone) .. 1 (pink noise:
    /// equal power per band).
    pub flatness: f32,
    pub bands: usize,
}

#[derive(Clone, Default)]
pub struct Outside {
    rate: u32,
    /// Smoothed display dB (tilt applied) of every grid band, from the bar's
    /// bottom to Nyquist; `db[0]` is grid band `first`.
    db: Vec<f32>,
    first: i32,
    pub lo: Side,
    pub hi: Side,
    /// Coarse FFT size, and whether another analyser paid for it.
    pub fft: usize,
    pub reused: bool,
    /// Bin widths, Hz, and where the fine source hands over.
    fine_hz: f32,
    coarse_hz: f32,
    handover: f32,
}

/// Grid band `i` covers MIDI `[i/BPS - 0.5, (i+1)/BPS - 0.5)`, so a note's
/// column holds whole bands and range edges fall on band edges.
fn band_midi(i: i32) -> (f32, f32) {
    (i as f32 / BPS - 0.5, (i + 1) as f32 / BPS - 0.5)
}

fn midi_hz(m: f32) -> f32 {
    pitch::midi_to_freq(m, A4)
}

impl Outside {
    pub fn update(&mut self, fine: &Source, coarse: &Source, rate: u32, range: NoteRange, fft: usize, reused: bool) {
        let first = ((NoteRange::BOUNDS.span().0 + 0.5) * BPS).round() as i32;
        let last = ((pitch::freq_to_midi(rate as f32 / 2.0, A4) + 0.5) * BPS).floor() as i32;
        let count = (last - first).max(0) as usize;
        let fresh = self.rate != rate || self.db.len() != count;
        (self.rate, self.first, self.fft, self.reused) = (rate, first, fft, reused);
        (self.fine_hz, self.coarse_hz, self.handover) = (fine.hz_per_bin, coarse.hz_per_bin, fine.top);
        if fresh {
            self.db = vec![FLOOR_DB; count];
        }
        let a = if fresh { 1.0 } else { SMOOTH };

        let readings: Vec<Reading> = (0..count)
            .map(|j| {
                let (m0, m1) = band_midi(first + j as i32);
                let hz = (midi_hz(m0), midi_hz(m1));
                let r = if hz.1 <= fine.top { fine.band(hz) } else { coarse.band(hz) };
                let tilt = TILT_DB_PER_OCT * ((hz.0 * hz.1).sqrt() / 1000.0).log2();
                let db = (10.0 * r.peak.max(1e-12).log10() + tilt).max(FLOOR_DB);
                self.db[j] += (db - self.db[j]) * a;
                r
            })
            .collect();

        let (s0, s1) = range.span();
        let nyq_m = band_midi(last).0;
        let idx = |m: f32| (((m + 0.5) * BPS).round() as i32 - first).clamp(0, count as i32) as usize;
        let (b0, b1) = (idx(s0), idx(s1));
        self.lo.update(&readings[..b0], (NoteRange::BOUNDS.span().0, s0), fresh);
        self.hi.update(&readings[b1..], (s1, nyq_m), fresh);
    }

    /// Hover text for whichever side `low` picks.
    pub fn describe(&self, low: bool) -> String {
        let s = if low { &self.lo } else { &self.hi };
        format!(
            "{} the range: {}–{} Hz · {} bands\nlevel {:.1} dB · peak {} Hz ({}) · centroid {} Hz\n{:.0}% tonal\nbins {:.2} Hz below {} Hz (fine), {:.1} Hz above ({}-pt, {})",
            if low { "Below" } else { "Above" },
            plot::fmt_hz(midi_hz(s.midi.0).round()),
            plot::fmt_hz(midi_hz(s.midi.1).round()),
            s.bands,
            s.level_db,
            plot::trim((s.peak * 10.0).round() / 10.0),
            pitch::note_name(pitch::freq_to_midi(s.peak.max(1.0), A4).round() as i32),
            plot::fmt_hz(s.centroid.round()),
            (1.0 - s.flatness) * 100.0,
            self.fine_hz,
            plot::fmt_hz(self.handover.round()),
            self.coarse_hz,
            self.fft,
            if self.reused { "shared" } else { "own" },
        )
    }

    /// Draw into the dimmed key areas. `keys` is the piano strip (the full
    /// [`NoteRange::BOUNDS`] span), `sel` the selection's x span, `air` the
    /// zone right of the keys for bar top .. Nyquist. Everything lines up
    /// with the keys; nothing moves when the selection does.
    pub fn paint(&self, p: &Painter, keys: Rect, sel: (f32, f32), air: Rect, range: NoteRange) {
        let full = NoteRange::BOUNDS.span();
        let (s0, s1) = range.span();
        let y = keys.y_range();
        self.paint_span(p, Rect::from_x_y_ranges(keys.left()..=sel.0, y), (full.0, s0), &self.lo, false);
        self.paint_span(p, Rect::from_x_y_ranges(sel.1..=keys.right(), y), (s1, full.1), &self.hi, true);
        self.paint_span(p, air, (full.1, self.hi.midi.1), &self.hi, false);
        let meter = |x: f32, s: &Side| {
            let m = Scale::linear((DB_SPAN.0, 0.0), (keys.bottom(), keys.top()));
            let top = m.px(s.level_db).clamp(keys.top(), keys.bottom());
            p.rect_filled(Rect::from_min_max(pos2(x, top), pos2(x + 3.0, keys.bottom())), 0.0, s.tone());
        };
        meter(keys.left(), &self.lo);
        meter(air.right() - 3.0, &self.hi);
    }

    /// Bands of `side` within MIDI `span`, across `rect` (linear in MIDI).
    fn paint_span(&self, p: &Painter, rect: Rect, span: (f32, f32), side: &Side, label_right: bool) {
        if rect.width() < 2.0 || span.1 <= span.0 || self.db.is_empty() {
            return;
        }
        let x = Scale::linear(span, (rect.left(), rect.right()));
        let y = Scale::linear(DB_SPAN, (rect.bottom(), rect.top()));
        // At most one column per pixel; each keeps its loudest band.
        let bands = ((span.1 - span.0) * BPS).round().max(1.0) as usize;
        let cols = bands.min(rect.width() as usize).max(1);
        let mut v = vec![f32::NEG_INFINITY; cols];
        for (j, &db) in self.db.iter().enumerate() {
            let (m0, m1) = band_midi(self.first + j as i32);
            let mid = (m0 + m1) / 2.0;
            if mid >= span.0 && mid < span.1 {
                let c = (((mid - span.0) / (span.1 - span.0)) * cols as f32) as usize;
                v[c.min(cols - 1)] = v[c.min(cols - 1)].max(db);
            }
        }
        let fill = side.tone().gamma_multiply(0.55);
        let w = rect.width() / cols as f32;
        for (c, &db) in v.iter().enumerate() {
            let top = y.px(db).clamp(rect.top(), rect.bottom());
            if top < rect.bottom() - 0.5 {
                let x0 = rect.left() + c as f32 * w;
                p.rect_filled(Rect::from_min_max(pos2(x0, top), pos2(x0 + w.max(1.0), rect.bottom())), 0.0, fill);
            }
        }
        if side.level_db > FLOOR_DB + 1.0 {
            let inside = |hz: f32| {
                let m = pitch::freq_to_midi(hz.max(1.0), A4);
                (m >= span.0 && m < span.1).then(|| x.px(m))
            };
            if let Some(px) = inside(side.peak) {
                p.vline(px, rect.y_range(), Stroke::new(1.0, theme::ON_ACCENT.gamma_multiply(0.8)));
            }
            if let Some(px) = inside(side.centroid) {
                p.circle_filled(pos2(px, rect.bottom() - 3.0), 2.0, Color32::WHITE);
            }
        }
        if rect.width() > 80.0 {
            let (at, align) = if label_right {
                (pos2(rect.right() - 5.0, rect.top() + 1.0), Align2::RIGHT_TOP)
            } else {
                (pos2(rect.left() + 5.0, rect.top() + 1.0), Align2::LEFT_TOP)
            };
            let text = format!("{:.0} dB · {}", side.level_db, plot::fmt_hz(side.peak.round()));
            p.text(at, align, text, FontId::monospace(9.0), Color32::from_gray(200));
        }
    }
}

impl Side {
    fn update(&mut self, bands: &[Reading], midi: (f32, f32), fresh: bool) {
        self.midi = midi;
        self.bands = bands.len();
        let a = if fresh { 1.0 } else { SMOOTH };
        let sum: f32 = bands.iter().map(|r| r.sum).sum();
        let level = (10.0 * (sum / HANN_SUM).max(1e-12).log10()).max(FLOOR_DB);
        self.level_db += (level - self.level_db) * a;
        if sum <= 1e-12 || bands.is_empty() {
            return;
        }
        let lnf = bands.iter().map(|r| r.sum * r.peak_hz.max(1.0).ln()).sum::<f32>() / sum;
        let ln_mean = bands.iter().map(|r| r.sum.max(1e-20).ln()).sum::<f32>() / bands.len() as f32;
        let flat = (ln_mean.exp() / (sum / bands.len() as f32)).clamp(0.0, 1.0);
        self.flatness += (flat - self.flatness) * a;
        let log_ema = |old: f32, new: f32| if old <= 0.0 || fresh { new } else { (old.ln() + (new.ln() - old.ln()) * a).exp() };
        self.centroid = log_ema(self.centroid, lnf.exp());
        // The peak glides while it stays within a semitone, else jumps.
        let peak = bands.iter().max_by(|x, y| x.peak.total_cmp(&y.peak)).map_or(0.0, |r| r.peak_hz);
        self.peak = if self.peak > 0.0 && (peak / self.peak).log2().abs() < 1.0 / 12.0 { log_ema(self.peak, peak) } else { peak };
    }

    /// Tonal content in the accent colour, noise in grey.
    fn tone(&self) -> Color32 {
        Color32::from_gray(150).lerp_to_gamma(theme::ACCENT_BRIGHT, 1.0 - self.flatness)
    }
}

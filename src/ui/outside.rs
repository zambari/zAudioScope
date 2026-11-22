//! The live spectrum along the shared note range, from the bar's lowest note
//! up to Nyquist, drawn over the range bar's keys (see
//! [`super::noterange::bar`]), plus stats for what the selection leaves out.
//!
//! Bands sit on a fixed quarter-tone grid in absolute pitch, so moving the
//! selection only changes which bands count as "outside", never where they
//! are. Each band reads from the finest spectrum that covers it: a 16×
//! decimated FFT (sub-Hz bins) for the low end, and for the rest the largest
//! FFT an analyser already ran this frame (`Shared::any_spectrum`) or one of
//! our own.

use egui::{Color32, Painter, Rect, pos2};

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
/// Width of the "air" zone (bar top .. Nyquist) right of the keys, px.
pub const AIR_W: f32 = 56.0;
/// Per-frame smoothing of bands and stats (1 = none).
const SMOOTH: f32 = 0.25;
/// A Hann-windowed sine spreads its power over ~1.5 bins' worth, so the
/// summed level of a lone full-scale sine reads 0 dB.
const HANN_SUM: f32 = 1.5;
/// Band dB mapped to the strip's height.
const DB_SPAN: (f32, f32) = (-84.0, -6.0);
/// Extra high-frequency emphasis for drawing only (on top of the shared
/// tilt), pivoting at `EMPHASIS_PIVOT` Hz, so the sparse top stays visible.
const EMPHASIS_DB_PER_OCT: f32 = 3.0;
const EMPHASIS_PIVOT: f32 = 200.0;

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
        Source {
            power,
            hz_per_bin: rate / n as f32,
            top,
        }
    }

    fn p(&self, k: usize) -> f32 {
        self.power
            .get(k.min(self.power.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    }

    fn lerp(&self, x: f32) -> f32 {
        let k = x.floor().max(0.0) as usize;
        let t = x - k as f32;
        self.p(k) * (1.0 - t) + self.p(k + 1) * t
    }

    fn band(&self, (f0, f1): (f32, f32)) -> Reading {
        let (b0, b1) = (f0 / self.hz_per_bin, f1 / self.hz_per_bin);
        let (k0, k1) = (
            b0.round() as usize,
            (b1.round() as usize).min(self.power.len()),
        );
        if k1 > k0 {
            let (mut sum, mut best) = (0.0, k0);
            for k in k0..k1 {
                sum += self.power[k];
                if self.power[k] > self.power[best] {
                    best = k;
                }
            }
            Reading {
                peak: self.power[best],
                sum,
                peak_hz: self.refine(best) * self.hz_per_bin,
            }
        } else {
            // Narrower than a bin: interpolate, and take its share of the bin.
            let c = (b0 + b1) / 2.0;
            let v = self.lerp(c);
            Reading {
                peak: v,
                sum: v * (b1 - b0),
                peak_hz: c * self.hz_per_bin,
            }
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
        if d < 0.0 {
            k as f32 + (0.5 * (a - c) / d).clamp(-0.5, 0.5)
        } else {
            k as f32
        }
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
    pub fn update(
        &mut self,
        fine: &Source,
        coarse: &Source,
        rate: u32,
        range: NoteRange,
        fft: usize,
        reused: bool,
    ) {
        let first = ((NoteRange::BOUNDS.span().0 + 0.5) * BPS).round() as i32;
        let last = ((pitch::freq_to_midi(rate as f32 / 2.0, A4) + 0.5) * BPS).floor() as i32;
        let count = (last - first).max(0) as usize;
        let fresh = self.rate != rate || self.db.len() != count;
        (self.rate, self.first, self.fft, self.reused) = (rate, first, fft, reused);
        (self.fine_hz, self.coarse_hz, self.handover) =
            (fine.hz_per_bin, coarse.hz_per_bin, fine.top);
        if fresh {
            self.db = vec![FLOOR_DB; count];
        }
        let a = if fresh { 1.0 } else { SMOOTH };

        let readings: Vec<Reading> = (0..count)
            .map(|j| {
                let (m0, m1) = band_midi(first + j as i32);
                let hz = (midi_hz(m0), midi_hz(m1));
                let r = if hz.1 <= fine.top {
                    fine.band(hz)
                } else {
                    coarse.band(hz)
                };
                let tilt = TILT_DB_PER_OCT * ((hz.0 * hz.1).sqrt() / 1000.0).log2();
                let db = (10.0 * r.peak.max(1e-12).log10() + tilt).max(FLOOR_DB);
                self.db[j] += (db - self.db[j]) * a;
                r
            })
            .collect();

        let (s0, s1) = range.span();
        let nyq_m = band_midi(last).0;
        let idx =
            |m: f32| (((m + 0.5) * BPS).round() as i32 - first).clamp(0, count as i32) as usize;
        let (b0, b1) = (idx(s0), idx(s1));
        self.lo
            .update(&readings[..b0], (NoteRange::BOUNDS.span().0, s0), fresh);
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

    /// Draw the spectrum over the keys. `keys` is the full
    /// [`NoteRange::BOUNDS`] span; `keys.right() .. right` is the "air" from
    /// the bar top to Nyquist. Gray, brighter towards the peaks; inside the
    /// selection only a faint trace. Nothing moves with the selection.
    pub fn paint(&self, p: &Painter, keys: Rect, right: f32, range: NoteRange) {
        let full = NoteRange::BOUNDS.span();
        let air = Rect::from_x_y_ranges(keys.right() + 2.0..=right, keys.y_range());
        p.rect_filled(air, 0.0, Color32::from_gray(14));
        self.paint_span(p, keys, full, range.span());
        self.paint_span(p, air, (full.1, self.hi.midi.1), range.span());
    }

    /// Bands within MIDI `span` across `rect` (linear in MIDI), with
    /// high-frequency emphasis; those inside `sel` almost invisible.
    fn paint_span(&self, p: &Painter, rect: Rect, span: (f32, f32), sel: (f32, f32)) {
        if rect.width() < 2.0 || span.1 <= span.0 || self.db.is_empty() {
            return;
        }
        // Height fraction per dB.
        let y = Scale::linear(DB_SPAN, (0.0, 1.0));
        // At most one column per pixel; each keeps its loudest band.
        let bands = ((span.1 - span.0) * BPS).round().max(1.0) as usize;
        let cols = bands.min(rect.width() as usize).max(1);
        let mut v = vec![(0.0f32, false); cols];
        for (j, &db) in self.db.iter().enumerate() {
            let (m0, m1) = band_midi(self.first + j as i32);
            let mid = (m0 + m1) / 2.0;
            if mid >= span.0 && mid < span.1 {
                let c =
                    ((((mid - span.0) / (span.1 - span.0)) * cols as f32) as usize).min(cols - 1);
                let db = db + EMPHASIS_DB_PER_OCT * (midi_hz(mid) / EMPHASIS_PIVOT).log2();
                v[c] = (
                    v[c].0.max(y.px(db).clamp(0.0, 1.0)),
                    mid >= sel.0 && mid < sel.1,
                );
            }
        }
        let w = rect.width() / cols as f32;
        for (c, &(t, inside)) in v.iter().enumerate() {
            if t * rect.height() < 0.5 {
                continue;
            }
            // A local maximum stands out: brighter fill and a white cap.
            let peak = (c == 0 || v[c - 1].0 < t) && v.get(c + 1).is_none_or(|n| n.0 <= t);
            let gray = Color32::from_gray((70.0 + 150.0 * t * t) as u8);
            let (fill, cap) = if peak {
                (theme::lerp(gray, Color32::WHITE, 0.35), Color32::WHITE)
            } else {
                (gray, theme::lerp(gray, Color32::WHITE, 0.3))
            };
            let alpha = if inside { 0.07 } else { 0.55 + 0.4 * t };
            let x0 = rect.left() + c as f32 * w;
            let top = rect.bottom() - t * rect.height();
            let col = Rect::from_min_max(pos2(x0, top), pos2(x0 + w.max(1.0), rect.bottom()));
            p.rect_filled(col, 0.0, fill.gamma_multiply(alpha));
            p.rect_filled(col.with_max_y(top + 1.0), 0.0, cap.gamma_multiply(alpha));
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
        let lnf = bands
            .iter()
            .map(|r| r.sum * r.peak_hz.max(1.0).ln())
            .sum::<f32>()
            / sum;
        let ln_mean = bands.iter().map(|r| r.sum.max(1e-20).ln()).sum::<f32>() / bands.len() as f32;
        let flat = (ln_mean.exp() / (sum / bands.len() as f32)).clamp(0.0, 1.0);
        self.flatness += (flat - self.flatness) * a;
        let log_ema = |old: f32, new: f32| {
            if old <= 0.0 || fresh {
                new
            } else {
                (old.ln() + (new.ln() - old.ln()) * a).exp()
            }
        };
        self.centroid = log_ema(self.centroid, lnf.exp());
        // The peak glides while it stays within a semitone, else jumps.
        let peak = bands
            .iter()
            .max_by(|x, y| x.peak.total_cmp(&y.peak))
            .map_or(0.0, |r| r.peak_hz);
        self.peak = if self.peak > 0.0 && (peak / self.peak).log2().abs() < 1.0 / 12.0 {
            log_ema(self.peak, peak)
        } else {
            peak
        };
    }
}

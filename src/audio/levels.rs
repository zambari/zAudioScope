//! Rolling histogram of short-block RMS levels, fed once per UI frame with
//! the audio that arrived since the last one. Every dB range control reads
//! the same precomputed distribution ([`LevelDist`]) instead of scanning
//! samples itself: "how much of the time is the signal below my floor /
//! above my ceiling" is then a lookup.
//!
//! Levels use the spectrum's scale: a full-scale sine reads 0 dB (block RMS
//! + 3 dB), so a ceiling at 0 dB is where a sine's spectral peak tops out.
//! Close enough for tonal material; broadband material spreads over many
//! FFT bins and reads lower there than here.

use std::rc::Rc;

use super::Frame;

/// Histogram span, dB. Levels outside land in the end bins.
const LO: f32 = -96.0;
const HI: f32 = 12.0;
/// Bins per dB.
const RES: f32 = 1.0;
const BINS: usize = ((HI - LO) * RES) as usize;
/// RMS block length, seconds.
const BLOCK_S: f32 = 0.02;
/// Memory of the histogram, seconds of audio (exponential).
const TAU_S: f32 = 3.0;

/// Fraction of recent blocks below each bin edge: `cdf[i]` covers levels
/// under `LO + i / RES` dB; `cdf[BINS] == 1` once anything was counted.
pub struct LevelDist {
    cdf: [f32; BINS + 1],
}

impl Default for LevelDist {
    fn default() -> Self {
        Self { cdf: [0.0; BINS + 1] }
    }
}

impl LevelDist {
    /// Fraction of recent time (0..1) the level sat below `db`.
    pub fn below(&self, db: f32) -> f32 {
        let x = ((db - LO) * RES).clamp(0.0, BINS as f32);
        let i = (x as usize).min(BINS - 1);
        let (a, b) = (self.cdf[i], self.cdf[i + 1]);
        a + (b - a) * (x - i as f32)
    }

    /// Fraction of recent time (0..1) the level sat above `db`.
    pub fn above(&self, db: f32) -> f32 {
        if self.cdf[BINS] > 0.0 { 1.0 - self.below(db) } else { 0.0 }
    }
}

/// The accumulator the app owns; [`Self::dist`] is what views read.
pub struct LevelHistogram {
    bins: [f32; BINS],
    /// Absolute sample position already consumed.
    pos: u64,
    /// Partial block: sum of squares and count.
    acc: (f64, usize),
    dist: Rc<LevelDist>,
}

impl Default for LevelHistogram {
    fn default() -> Self {
        Self { bins: [0.0; BINS], pos: 0, acc: (0.0, 0), dist: Rc::default() }
    }
}

impl LevelHistogram {
    /// Consume the part of `frame` not seen yet. `reset` (stream restarted)
    /// forgets the history. The distribution is rebuilt only when at least
    /// one block completed.
    pub fn ingest(&mut self, frame: &Frame, reset: bool) {
        if reset || self.pos > frame.end() {
            *self = Self::default();
        }
        let from = self.pos.max(frame.start);
        let new = &frame.samples[(from - frame.start) as usize..];
        self.pos = frame.end();
        let block = ((frame.rate as f32 * BLOCK_S) as usize).max(1);
        let done = (self.acc.1 + new.len()) / block;
        if done > 0 {
            // Age the old blocks first so the new ones count fully.
            let decay = (-(done as f32) * BLOCK_S / TAU_S).exp();
            self.bins.iter_mut().for_each(|b| *b *= decay);
        }
        for &s in new {
            self.acc.0 += (s as f64) * (s as f64);
            self.acc.1 += 1;
            if self.acc.1 >= block {
                // Sine-peak scale: RMS² × 2 → 0 dB for a full-scale sine.
                let ms = (2.0 * self.acc.0 / self.acc.1 as f64) as f32;
                let db = 10.0 * ms.max(1e-20).log10();
                self.bins[(((db - LO) * RES).max(0.0) as usize).min(BINS - 1)] += 1.0;
                self.acc = (0.0, 0);
            }
        }
        if done == 0 {
            return;
        }
        let total: f32 = self.bins.iter().sum();
        let mut dist = LevelDist::default();
        let mut run = 0.0;
        for (i, b) in self.bins.iter().enumerate() {
            run += b;
            dist.cdf[i + 1] = run / total;
        }
        self.dist = Rc::new(dist);
    }

    pub fn dist(&self) -> Rc<LevelDist> {
        self.dist.clone()
    }
}

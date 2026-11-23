//! Note math and pitch detection.
//!
//! For bass, plain FFT peak-picking is weak: at 48 kHz a 8192-point FFT has
//! ~5.9 Hz bins, while E1 (41.2 Hz) to F1 is only ~2.4 Hz apart, and the
//! fundamental is often quieter than the 2nd harmonic. Time-domain methods
//! (autocorrelation / NSDF, as in McLeod's MPM) resolve low notes far better
//! from a short window, so that is the default. FFT stays selectable for
//! material where it behaves better: clear harmonic tones above the NSDF
//! ceiling, missing fundamentals, noisy or harmonically dense sources. It
//! scores each possible fundamental by how much of the spectrum's peak
//! energy its harmonic series explains.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::audio::spectrum::Fft;

pub const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// MIDI note number (fractional) for a frequency, A4 = 440 Hz = 69.
pub fn freq_to_midi(f: f32, a4: f32) -> f32 {
    69.0 + 12.0 * (f / a4).log2()
}

pub fn midi_to_freq(m: f32, a4: f32) -> f32 {
    a4 * 2f32.powf((m - 69.0) / 12.0)
}

pub fn note_name(midi: i32) -> String {
    format!(
        "{}{}",
        NOTE_NAMES[midi.rem_euclid(12) as usize],
        midi.div_euclid(12) - 1
    )
}

/// Note name without the octave ("D#").
pub fn class_name(midi: i32) -> &'static str {
    NOTE_NAMES[midi.rem_euclid(12) as usize]
}

/// Distance in semitones between two pitches, ignoring whole octaves (0..6).
pub fn octave_free_gap(a: f32, b: f32) -> f32 {
    let d = a - b;
    (d - 12.0 * (d / 12.0).round()).abs()
}

/// Nearest enabled target note to `midi`, and the offset in cents
/// (positive: the input is sharp, tune down).
pub fn snap(midi: f32, enabled: impl IntoIterator<Item = i32>) -> Option<(i32, f32)> {
    enabled
        .into_iter()
        .map(|n| (n, (midi - n as f32) * 100.0))
        .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Method {
    Autocorrelation,
    Fft,
}

impl Method {
    pub const ALL: [Method; 2] = [Method::Autocorrelation, Method::Fft];

    pub fn label(self) -> &'static str {
        match self {
            Method::Autocorrelation => "Autocorr",
            Method::Fft => "FFT",
        }
    }
}

/// Detected fundamental in Hz, or `None` for silence / no clear pitch.
#[allow(dead_code)] // the tuner reads the full candidate list instead
pub fn detect(method: Method, samples: &[f32], rate: u32, f_min: f32, f_max: f32) -> Option<f32> {
    method.candidates(samples, rate, f_min, f_max).pick()
}

impl Method {
    /// All pitch candidates `method` finds in `samples` between `f_min` and `f_max`.
    pub fn candidates(self, samples: &[f32], rate: u32, f_min: f32, f_max: f32) -> Candidates {
        match self {
            Method::Autocorrelation => candidates(samples, rate, f_min, f_max),
            Method::Fft => fft_candidates(samples, rate, f_min, f_max),
        }
    }
}

/// One possible fundamental: a peak of the NSDF curve, or a harmonic series
/// in the spectrum.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub freq: f32,
    /// 0..1, how sure: NSDF peak height (how periodic the signal is at this
    /// lag), or the share of spectral peak energy the harmonics explain.
    pub clarity: f32,
}

/// All candidates in the range, plus which one is the pitch. NSDF lists its
/// key maxima by ascending lag (descending frequency) and picks by McLeod's
/// rule; FFT lists the notes it found, the one explaining the most of the
/// sound first, and picks that.
#[derive(Default, Debug)]
pub struct Candidates {
    pub list: Vec<Candidate>,
    pub pick: Option<usize>,
}

impl Candidates {
    pub fn pick(&self) -> Option<f32> {
        self.pick.map(|i| self.list[i].freq)
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// Peak of the parabola through three equally spaced samples around a local
/// maximum `b`: (offset from `b`'s position, -0.5..0.5; interpolated height).
fn parabolic(a: f32, b: f32, c: f32) -> (f32, f32) {
    let denom = a - 2.0 * b + c;
    let shift = if denom.abs() > 1e-9 {
        (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    (shift, b - 0.25 * (a - c) * shift)
}

/// Dot product with 8 independent running sums. A plain `zip().sum()` must
/// add in order (float addition isn't associative), which keeps it scalar;
/// separate lanes let the compiler use SIMD, ~8× faster for the NSDF.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut s = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: f32 = ca
        .remainder()
        .iter()
        .zip(cb.remainder())
        .map(|(x, y)| x * y)
        .sum();
    for (p, q) in ca.zip(cb) {
        for k in 0..8 {
            s[k] += p[k] * q[k];
        }
    }
    s.iter().sum::<f32>() + tail
}

/// Normalized square difference (McLeod). Each positive lobe of the curve
/// contributes its maximum, refined by parabolic interpolation; the pick is
/// the first one close to the global best, which avoids octave errors.
pub fn candidates(x: &[f32], rate: u32, f_min: f32, f_max: f32) -> Candidates {
    let mut out = Candidates::default();
    let n = x.len();
    if rms(x) < 1e-3 {
        return out;
    }
    let lag_min = (rate as f32 / f_max).floor().max(2.0) as usize;
    let lag_max = ((rate as f32 / f_min).ceil() as usize).min(n / 2);
    if lag_max <= lag_min + 2 {
        return out;
    }
    // m(tau) = sum of x[i]^2 + x[i+tau]^2 over the overlap, updated
    // incrementally as the overlap shrinks, so each lag costs one dot product.
    let mut m: f32 = 2.0 * x.iter().map(|s| s * s).sum::<f32>();
    let curve: Vec<f32> = (0..=lag_max + 1)
        .map(|tau| {
            if tau > 0 {
                m -= x[n - tau] * x[n - tau] + x[tau - 1] * x[tau - 1];
            }
            let acf = dot(&x[..n - tau], &x[tau..]);
            if m > 0.0 { 2.0 * acf / m } else { 0.0 }
        })
        .collect();

    // Walk the lobes. The first one (around lag 0) is the signal matching
    // itself, so start after the curve first goes negative.
    let mut t = 1;
    while t <= lag_max && curve[t] > 0.0 {
        t += 1;
    }
    let mut lobe: Option<usize> = None;
    let push = |i: usize, out: &mut Candidates| {
        if i < lag_min.max(1) || i > lag_max {
            return;
        }
        let (shift, peak) = parabolic(curve[i - 1], curve[i], curve[i + 1]);
        out.list.push(Candidate {
            freq: rate as f32 / (i as f32 + shift),
            clarity: peak.clamp(0.0, 1.0),
        });
    };
    while t <= lag_max {
        let v = curve[t];
        if v > 0.0 {
            lobe = Some(match lobe {
                Some(b) if curve[b] >= v => b,
                _ => t,
            });
        } else if let Some(b) = lobe.take() {
            push(b, &mut out);
        }
        t += 1;
    }
    // A lobe still open at the end only counts if its peak is interior.
    if let Some(b) = lobe
        && curve[b] > curve[b + 1]
    {
        push(b, &mut out);
    }

    let best = out.list.iter().map(|c| c.clarity).fold(0f32, f32::max);
    if best >= 0.5 {
        out.pick = out.list.iter().position(|c| c.clarity >= 0.9 * best);
    }
    out
}

/// Spectral peaks further than this below the loudest are ignored.
const PEAK_RANGE_DB: f32 = 50.0;
/// Only peaks within this of the loudest set how many harmonics a
/// fundamental is expected to show.
const STRONG_DB: f32 = 30.0;
/// Loudest spectral peaks considered.
const MAX_PEAKS: usize = 40;
/// Harmonics scored per candidate fundamental.
const MAX_HARMONIC: usize = 12;
/// A fundamental may be this many times below its lowest present peak
/// (missing or weak fundamental).
const MAX_SUBDIV: usize = 4;
/// A harmonic slot matches a peak within this many cents (bass strings run
/// a little sharp up the series).
const HARMONIC_CENTS: f32 = 40.0;
/// Notes found per analysis at most (one per peeling round).
const MAX_NOTES: usize = 4;
/// A later note must explain at least this share of all peak energy
/// (≈ -17 dB), so leftovers (sidelobes, noise, inharmonic partials) of a
/// single note don't turn into extra ones.
const MIN_SHARE: f32 = 0.02;
/// Peeling stops once the best remaining note is less clear than this.
const MIN_ROUND_CLARITY: f32 = 0.3;
/// A later note this close (cents) to 1/2…1/4 of an earlier one is that
/// note's subharmonic: the earlier note should have been it, so it's skipped.
const SUBHARMONIC_CENTS: f32 = 30.0;
/// First notes tried; each is peeled to the end and the best path kept.
const FIRST_CHOICES: usize = 6;
/// Path score cost per extra note: two notes must explain clearly more
/// than one (a lone tone with a weak fundamental stays one note).
const NOTE_COST: f32 = 0.05;

thread_local! {
    /// FFT plans by size, per thread (the tuner runs on its own worker).
    static PLANS: RefCell<HashMap<usize, Fft>> = RefCell::new(HashMap::new());
}

/// Harmonic-sum pitch on the spectrum, several notes at once. Peaks of a
/// zero-padded Hann spectrum are found and refined; each peak (and its
/// 1/2…1/4) is a candidate fundamental, scored by the share of the
/// remaining peak energy its harmonic series explains, times the square
/// root of the share of its expected harmonic slots that are filled (which
/// sinks subharmonics: half their slots are empty).
///
/// A note is taken and the peaks it explains are marked used; then the rest
/// is scored again, up to [`MAX_NOTES`]. Used peaks still fill a later
/// note's slots (a fifth above shares harmonics) but add no energy, so the
/// first note's octaves and subharmonics find nothing left and drop out,
/// while a second, independent note scores as clearly as if it played
/// alone. Greedy alone would take C2 for a C3+G3 dyad (their common
/// fundamental explains every peak), so the first note is chosen by
/// lookahead: each of the best [`FIRST_CHOICES`] is peeled to the end, and
/// the path explaining the most energy (filled-slot weighted, minus
/// [`NOTE_COST`] per extra note) wins. Listed loudest-explaining first; the
/// frequency is the energy-weighted mean of the matched `f_h / h`.
///
/// The flip side: a tone with no fundamental at all (A3 as 440/660/880…)
/// is the same spectrum as an A4+E5 dyad and reads as that.
pub fn fft_candidates(x: &[f32], rate: u32, f_min: f32, f_max: f32) -> Candidates {
    let mut out = Candidates::default();
    let n = x.len();
    if n < 64 || rms(x) < 1e-3 {
        return out;
    }
    // Zero-pad to at least twice the window: finer bins for the peak fit.
    let size = (2 * n).next_power_of_two().max(1024);
    let mut buf = vec![0f32; size];
    let h = std::f32::consts::TAU / n as f32;
    for (i, (b, s)) in buf.iter_mut().zip(x).enumerate() {
        *b = s * (0.5 - 0.5 * (h * i as f32).cos());
    }
    let power = PLANS.with(|p| {
        p.borrow_mut()
            .entry(size)
            .or_insert_with(|| Fft::new(size))
            .power_prewindowed(&buf)
    });
    let bin_hz = rate as f32 / size as f32;
    let db: Vec<f32> = power.iter().map(|p| 10.0 * p.max(1e-30).log10()).collect();

    // Peaks from just under the lowest fundamental up to near Nyquist.
    let k_lo = ((0.9 * f_min / bin_hz) as usize).max(1);
    let k_hi = ((0.45 * rate as f32 / bin_hz) as usize).min(db.len() - 2);
    if k_hi <= k_lo + 2 {
        return out;
    }
    let loudest = db[k_lo..=k_hi].iter().copied().fold(f32::MIN, f32::max);
    // (freq, linear power, dB)
    let mut peaks: Vec<(f32, f32, f32)> = (k_lo..=k_hi)
        .filter(|&k| db[k] > db[k - 1] && db[k] >= db[k + 1] && db[k] > loudest - PEAK_RANGE_DB)
        .map(|k| {
            let (shift, peak) = parabolic(db[k - 1], db[k], db[k + 1]);
            ((k as f32 + shift) * bin_hz, 10f32.powf(peak / 10.0), peak)
        })
        .collect();
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    peaks.truncate(MAX_PEAKS);
    let total: f32 = peaks.iter().map(|p| p.1).sum();
    let f_strong = peaks
        .iter()
        .filter(|p| p.2 > loudest - STRONG_DB)
        .map(|p| p.0)
        .fold(0f32, f32::max);
    let tol = 2f32.powf(HARMONIC_CENTS / 1200.0) - 1.0;

    // (candidate, unused energy explained, indices of the peaks it matched)
    let score = |f0: f32, used: &[bool], remaining: f32| -> Option<(Candidate, f32, Vec<usize>)> {
        let (mut energy, mut weighted, mut slots) = (0f32, 0f32, 0usize);
        let mut matched = Vec::new();
        for h in 1..=MAX_HARMONIC {
            let target = f0 * h as f32;
            if target > 0.45 * rate as f32 || (h > 1 && target > f_strong * (1.0 + tol)) {
                break;
            }
            slots += 1;
            let width = (target * tol).max(1.5 * bin_hz);
            let best = (0..peaks.len())
                .filter(|&i| (peaks[i].0 - target).abs() <= width)
                .max_by(|&a, &b| peaks[a].1.total_cmp(&peaks[b].1));
            if let Some(i) = best {
                matched.push(i);
                let (f, p, _) = peaks[i];
                if !used[i] {
                    energy += p;
                    weighted += p * f / h as f32;
                }
            }
        }
        (energy > 0.0).then(|| {
            let fill = (matched.len() as f32 / slots as f32).sqrt();
            let clarity = (energy / remaining * fill).clamp(0.0, 1.0);
            (
                Candidate {
                    freq: weighted / energy,
                    clarity,
                },
                energy,
                matched,
            )
        })
    };
    let f0s: Vec<f32> = peaks
        .iter()
        .flat_map(|p| (1..=MAX_SUBDIV).map(move |d| p.0 / d as f32))
        .filter(|&f| f >= f_min && f <= f_max)
        .collect();
    let in_range = |c: &Candidate| c.freq >= f_min && c.freq <= f_max;
    // Peel notes off greedily after `first`: ((note, share of all energy
    // it explains), path score).
    let peel = |first: f32| -> (Vec<(Candidate, f32)>, f32) {
        let mut used = vec![false; peaks.len()];
        let (mut notes, mut path) = (Vec::new(), 0f32);
        let mut next = Some(first);
        while notes.len() < MAX_NOTES {
            let remaining: f32 = (0..peaks.len())
                .filter(|&i| !used[i])
                .map(|i| peaks[i].1)
                .sum();
            if remaining <= MIN_SHARE * total {
                break;
            }
            let subharmonic = |f: f32| {
                notes.iter().any(|(n, _): &(Candidate, f32)| {
                    let r = n.freq / f;
                    let k = r.round();
                    (2.0..=MAX_SUBDIV as f32).contains(&k)
                        && (1200.0 * (r / k).log2()).abs() < SUBHARMONIC_CENTS
                })
            };
            let scored = |f: f32| {
                score(f, &used, remaining).filter(|(c, ..)| in_range(c) && !subharmonic(c.freq))
            };
            let best = match next.take() {
                Some(f) => scored(f),
                None => f0s
                    .iter()
                    .filter_map(|&f| scored(f))
                    .max_by(|a, b| a.0.clarity.total_cmp(&b.0.clarity)),
            };
            let Some((c, energy, matched)) = best else {
                break;
            };
            // A weak first note would let the next one sweep up the rest.
            if c.clarity < MIN_ROUND_CLARITY || energy < MIN_SHARE * total {
                break;
            }
            // Clarity is relative to what was left; the path counts the
            // share of everything, so later notes weigh by their loudness.
            let share = c.clarity * remaining / total;
            path += share - if notes.is_empty() { 0.0 } else { NOTE_COST };
            matched.into_iter().for_each(|i| used[i] = true);
            notes.push((c, share));
        }
        (notes, path)
    };
    let none = vec![false; peaks.len()];
    let mut firsts: Vec<(f32, f32)> = f0s
        .iter()
        .filter_map(|&f| {
            score(f, &none, total)
                .filter(|(c, ..)| in_range(c))
                .map(|(c, ..)| (f, c.clarity))
        })
        .collect();
    firsts.sort_by(|a, b| b.1.total_cmp(&a.1));
    // Near-identical starting points peel the same way: one each.
    firsts.dedup_by(|a, b| (a.0 / b.0 - 1.0).abs() < tol);
    let mut notes = firsts
        .iter()
        .take(FIRST_CHOICES)
        .map(|&(f, _)| peel(f))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|p| p.0)
        .unwrap_or_default();
    // The note explaining the most of the sound first: that is the pick.
    notes.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.list = notes.into_iter().map(|n| n.0).collect();
    if out.list.first().is_some_and(|c| c.clarity >= 0.5) {
        out.pick = Some(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(f: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (std::f32::consts::TAU * f * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn detects_low_e() {
        let f = detect(
            Method::Autocorrelation,
            &sine(41.2, 48_000, 8192),
            48_000,
            30.0,
            500.0,
        )
        .unwrap();
        assert!((f - 41.2).abs() < 0.1, "{f}");
    }

    /// Sum of harmonics `(number, amplitude)` of `f`.
    fn tone(f: f32, harmonics: &[(usize, f32)], rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = std::f32::consts::TAU * f * i as f32 / rate as f32;
                harmonics
                    .iter()
                    .map(|&(h, a)| a * (h as f32 * t).sin())
                    .sum()
            })
            .collect()
    }

    fn fft_pick(x: &[f32], rate: u32, f_min: f32, f_max: f32) -> f32 {
        let c = fft_candidates(x, rate, f_min, f_max);
        c.pick().unwrap_or_else(|| panic!("no pitch: {:?}", c.list))
    }

    #[test]
    fn fft_high_harmonic_tone() {
        // A6, above the NSDF ceiling, with a rich series.
        let x = tone(
            1760.0,
            &[(1, 0.3), (2, 0.2), (3, 0.1), (4, 0.05)],
            48_000,
            14_400,
        );
        let f = fft_pick(&x, 48_000, 30.0, 8000.0);
        assert!((1200.0 * (f / 1760.0).log2()).abs() < 3.0, "{f}");
        // One note: no octave or subharmonic ghosts beside it.
        let c = fft_candidates(&x, 48_000, 30.0, 8000.0);
        assert_eq!(
            c.list.iter().filter(|k| k.clarity >= 0.5).count(),
            1,
            "{:?}",
            c.list
        );
    }

    #[test]
    fn fft_missing_fundamental_keeps_the_note_name() {
        // No energy at 220 Hz itself. That spectrum is also an A4+E5 dyad
        // (440/880 + 660) and chords win that tie, but the pick is still A.
        let x = tone(
            220.0,
            &[(2, 0.4), (3, 0.2), (4, 0.1), (5, 0.05)],
            48_000,
            14_400,
        );
        let f = fft_pick(&x, 48_000, 30.0, 4000.0);
        let semis = freq_to_midi(f, 440.0) - 69.0;
        assert!((semis - 12.0 * (semis / 12.0).round()).abs() < 0.05, "{f}");
    }

    #[test]
    fn fft_low_e_decimated() {
        let x = tone(41.2, &[(1, 0.3), (2, 0.4), (3, 0.2)], 3_000, 900);
        let f = fft_pick(&x, 3_000, 30.0, 300.0);
        assert!((1200.0 * (f / 41.2).log2()).abs() < 5.0, "{f}");
    }

    #[test]
    fn fft_two_notes_a_fifth_apart() {
        // C3 and G3 together share harmonics (C's 3rd is G's 2nd); both
        // should come out clear, and no octave ghost of either.
        let rich = [(1, 0.3), (2, 0.2), (3, 0.12), (4, 0.08), (5, 0.05)];
        let (c, g) = (130.81, 196.0);
        let x: Vec<f32> = tone(c, &rich, 48_000, 14_400)
            .iter()
            .zip(tone(g, &rich, 48_000, 14_400))
            .map(|(a, b)| a + b)
            .collect();
        let found = fft_candidates(&x, 48_000, 30.0, 2000.0);
        let clear: Vec<f32> = found
            .list
            .iter()
            .filter(|k| k.clarity >= 0.5)
            .map(|k| k.freq)
            .collect();
        let near = |f: f32| clear.iter().any(|&k| (1200.0 * (k / f).log2()).abs() < 5.0);
        assert!(near(c) && near(g), "{:?}", found.list);
        assert_eq!(clear.len(), 2, "{:?}", found.list);
    }

    #[test]
    fn snaps_to_nearest_enabled() {
        // C2=36, D2=38, G2=43; 37.4 is nearer D2, 60 cents flat.
        let (n, cents) = snap(37.4, [36, 38, 43]).unwrap();
        assert_eq!(n, 38);
        assert!((cents + 60.0).abs() < 1e-3);
    }

    #[test]
    fn names() {
        assert_eq!(note_name(69), "A4");
        assert_eq!(note_name(28), "E1");
    }
}

/// Baseline cost of the NSDF at realistic sizes, to judge whether moving it
/// elsewhere (GPU, FFT) is worth it. Run with
/// `cargo test --release bench_nsdf -- --ignored --nocapture`.
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn bench_nsdf() {
        for (rate, window_ms) in [
            (12_000, 300.0),
            (12_000, 1000.0),
            (48_000, 300.0),
            (48_000, 1000.0),
        ] {
            let n = (rate as f32 * window_ms / 1000.0) as usize;
            let x: Vec<f32> = (0..n)
                .map(|i| (std::f32::consts::TAU * 41.2 * i as f32 / rate as f32).sin() * 0.5)
                .collect();
            let runs = 50;
            let t = Instant::now();
            for _ in 0..runs {
                std::hint::black_box(candidates(std::hint::black_box(&x), rate, 30.0, 1357.0));
            }
            let us = t.elapsed().as_secs_f64() * 1e6 / runs as f64;
            let lags = (rate as f32 / 30.0).ceil() as usize;
            // The bare lag sweep with the old scalar dot product, for comparison.
            let t = Instant::now();
            for _ in 0..runs {
                let x = std::hint::black_box(&x);
                let mut acc = 0f32;
                for tau in 0..=lags {
                    acc += x[..n - tau]
                        .iter()
                        .zip(&x[tau..])
                        .map(|(a, b)| a * b)
                        .sum::<f32>();
                }
                std::hint::black_box(acc);
            }
            let scalar_us = t.elapsed().as_secs_f64() * 1e6 / runs as f64;
            println!(
                "rate {rate:>6} window {window_ms:>5} ms: n={n:>6} lags={lags:>5} -> {us:>9.1} µs ({:.2} GMAC/s)",
                (n * lags) as f64 / us / 1e3
            );
            println!(
                "    old scalar sweep:   {scalar_us:>9.1} µs ({:.1}x slower)",
                scalar_us / us
            );
            let t = Instant::now();
            for _ in 0..runs {
                std::hint::black_box(fft_candidates(std::hint::black_box(&x), rate, 30.0, 8000.0));
            }
            let fft_us = t.elapsed().as_secs_f64() * 1e6 / runs as f64;
            println!("    fft harmonic peel:  {fft_us:>9.1} µs");
        }
    }
}

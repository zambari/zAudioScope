//! Note math and pitch detection.
//!
//! For bass, plain FFT peak-picking is weak: at 48 kHz a 8192-point FFT has
//! ~5.9 Hz bins, while E1 (41.2 Hz) to F1 is only ~2.4 Hz apart, and the
//! fundamental is often quieter than the 2nd harmonic. Time-domain methods
//! (autocorrelation / NSDF, as in McLeod's MPM) resolve low notes far better
//! from a short window, so that is the default. FFT stays selectable for
//! material where it behaves better (e.g. noisy, harmonically dense sources).

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
    match method {
        Method::Autocorrelation => candidates(samples, rate, f_min, f_max).pick(),
        // TODO: FFT + harmonic product spectrum, with parabolic peak
        // interpolation. Until then, fall back so the selector is usable.
        Method::Fft => candidates(samples, rate, f_min, f_max).pick(),
    }
}

/// One periodicity the autocorrelation found: a peak of the NSDF curve.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub freq: f32,
    /// NSDF peak height, 0..1: how periodic the signal is at this lag.
    pub clarity: f32,
}

/// All NSDF key maxima in the lag range, by ascending lag (descending
/// frequency), plus which one McLeod's rule picks as the pitch.
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

/// Normalized square difference (McLeod). Each positive lobe of the curve
/// contributes its maximum, refined by parabolic interpolation; the pick is
/// the first one close to the global best, which avoids octave errors.
pub fn candidates(x: &[f32], rate: u32, f_min: f32, f_max: f32) -> Candidates {
    let mut out = Candidates::default();
    let n = x.len();
    let rms = (x.iter().map(|s| s * s).sum::<f32>() / n.max(1) as f32).sqrt();
    if rms < 1e-3 {
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
            let acf: f32 = x[..n - tau].iter().zip(&x[tau..]).map(|(a, b)| a * b).sum();
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
        let (a, b, c) = (curve[i - 1], curve[i], curve[i + 1]);
        let denom = a - 2.0 * b + c;
        let shift = if denom.abs() > 1e-9 {
            (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let peak = b - 0.25 * (a - c) * shift;
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

//! Peak and ITU-R BS.1770 loudness of a stretch of mono samples.

/// One biquad section, direct form I, in f64 so long sums stay accurate.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    fn run(&self, x: &mut [f64]) {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for s in x.iter_mut() {
            let y =
                self.b[0] * *s + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
            (x2, x1, y2, y1) = (x1, *s, y1, y);
            *s = y;
        }
    }
}

/// The two K-weighting stages (high shelf, then high pass) for `rate`,
/// computed from the BS.1770 analogue prototypes so any rate works.
fn k_weighting(rate: u32) -> [Biquad; 2] {
    let fs = rate as f64;
    let shelf = {
        let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let vh = 10f64.powf(g / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b: [
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        }
    };
    let highpass = {
        let (f0, q) = (38.13547087602444, 0.5003270373238773);
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        Biquad {
            b: [1.0, -2.0, 1.0],
            a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        }
    };
    [shelf, highpass]
}

/// Sample peak and loudness of `samples[from..to]`, as linear peak and LUFS.
/// The filters start `settle` samples earlier (when there are any) so their
/// start-up transient doesn't count. Loudness is ungated, single channel:
/// -0.691 + 10·log10(mean square of the K-weighted signal).
pub fn measure(
    samples: &[f32],
    rate: u32,
    from: usize,
    to: usize,
    settle: usize,
) -> Option<(f32, f32)> {
    let to = to.min(samples.len());
    if from >= to {
        return None;
    }
    let peak = samples[from..to].iter().fold(0f32, |m, s| m.max(s.abs()));
    let pre = from.min(settle);
    let mut x: Vec<f64> = samples[from - pre..to].iter().map(|&s| s as f64).collect();
    for f in k_weighting(rate) {
        f.run(&mut x);
    }
    let body = &x[pre..];
    let ms = body.iter().map(|y| y * y).sum::<f64>() / body.len() as f64;
    let lufs = if ms > 0.0 {
        (-0.691 + 10.0 * ms.log10()) as f32
    } else {
        f32::NEG_INFINITY
    };
    Some((peak, lufs))
}

/// Linear amplitude to dB; silence is -inf.
pub fn db(v: f32) -> f32 {
    if v > 0.0 {
        20.0 * v.log10()
    } else {
        f32::NEG_INFINITY
    }
}

/// dB to linear amplitude.
pub fn lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BS.1770: a 0 dBFS 997 Hz sine in one channel reads -3.01 LUFS.
    #[test]
    fn sine_997hz() {
        let rate = 48_000;
        let x: Vec<f32> = (0..rate * 2)
            .map(|i| (2.0 * std::f32::consts::PI * 997.0 * i as f32 / rate as f32).sin())
            .collect();
        let (peak, lufs) = measure(
            &x,
            rate as u32,
            rate as usize / 2,
            x.len(),
            rate as usize / 2,
        )
        .unwrap();
        assert!((peak - 1.0).abs() < 1e-3);
        assert!((lufs + 3.01).abs() < 0.05, "{lufs}");
    }
}

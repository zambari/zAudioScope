//! Polar note geometry: where a MIDI key sits on a clock face. Pure maths,
//! no drawing. Angles run from north; radii are fractions of the face
//! (0 = centre, 1 = outer edge).
//!
//! - Pitch-class order: chromatic, or the circle of fifths (`pc·7 mod 12`).
//!   In fifths mode a spiral still climbs by pitch, so neighbouring notes
//!   jump around within each octave; that is the nature of fifths.
//! - [`Radial`]: every octave folded onto one ring, one ring per octave, or
//!   a spiral whose radius grows continuously with pitch.
//! - Octaves start at the pitch class at north (`offset`), so rings and the
//!   spiral meet at the top.
//! - `depth` is the octave (fractional), kept for a 3D view later.

use std::f32::consts::TAU;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Radial {
    Single,
    Rings,
    Spiral,
}

impl Radial {
    pub const ALL: [Radial; 3] = [Radial::Single, Radial::Rings, Radial::Spiral];

    pub fn label(self) -> &'static str {
        match self {
            Radial::Single => "single ring",
            Radial::Rings => "ring per octave",
            Radial::Spiral => "spiral",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// Circle of fifths instead of chromatic order.
    pub fifths: bool,
    /// Pitch class at north, 0 = C.
    pub offset: i32,
    pub clockwise: bool,
    pub radial: Radial,
    /// High notes in the centre instead of on the outside.
    pub invert: bool,
    /// Keys shown, inclusive.
    pub lo: i32,
    pub hi: i32,
    /// Radius of the innermost ring / spiral start, 0..1.
    pub inner: f32,
    /// Extra rotation of the whole dial, radians, clockwise.
    pub turn: f32,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            fifths: false,
            offset: 0,
            clockwise: true,
            radial: Radial::Single,
            invert: false,
            lo: 36,
            hi: 83,
            inner: 0.35,
            turn: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    /// Radians from north; positive turns clockwise on screen.
    pub angle: f32,
    /// 0..1 of the face radius.
    pub radius: f32,
    /// Octaves above the first octave shown (fractional).
    pub depth: f32,
}

impl Spot {
    /// Screen offset from the centre for a face of `r` pixels (y down).
    pub fn offset(&self, r: f32) -> egui::Vec2 {
        egui::vec2(self.angle.sin(), -self.angle.cos()) * self.radius * r
    }
}

impl Layout {
    /// Position of pitch class `pc` around the dial, 0..12 steps from north.
    pub fn step(&self, pc: i32) -> i32 {
        let order = |pc: i32| if self.fifths { (pc * 7).rem_euclid(12) } else { pc.rem_euclid(12) };
        (order(pc) - order(self.offset)).rem_euclid(12)
    }

    /// Angle of pitch class `pc`, radians from north (signed by direction).
    pub fn angle(&self, pc: i32) -> f32 {
        let a = self.step(pc) as f32 / 12.0 * TAU;
        (if self.clockwise { a } else { -a }) + self.turn
    }

    /// Octave index of `key`; octaves begin at the pitch class at north.
    fn octave(&self, key: i32) -> i32 {
        (key - self.offset).div_euclid(12)
    }

    /// Number of (partly) shown octaves.
    pub fn octaves(&self) -> i32 {
        self.octave(self.hi) - self.octave(self.lo) + 1
    }

    /// Radial distance between neighbouring rings (or spiral turns), as a
    /// fraction of the face; for a single ring, a band width that reads well.
    pub fn spacing(&self) -> f32 {
        match self.radial {
            Radial::Single => 0.2,
            Radial::Rings => (1.0 - self.inner) / (self.octaves() - 1).max(1) as f32,
            Radial::Spiral => (1.0 - self.inner) / self.octaves() as f32,
        }
    }

    pub fn place(&self, key: i32) -> Spot {
        let base = self.octave(self.lo);
        let depth = (key - self.offset - 12 * base) as f32 / 12.0;
        let n = self.octaves();
        let u = match self.radial {
            Radial::Single => 1.0,
            Radial::Rings if n <= 1 => 1.0,
            Radial::Rings => (self.octave(key) - base) as f32 / (n - 1) as f32,
            // Whole octaves, so each turn is the same width.
            Radial::Spiral => depth / (n as f32 - 1.0 / 12.0),
        };
        let mut radius = self.inner + (1.0 - self.inner) * u;
        if self.invert && self.radial != Radial::Single {
            radius = 1.0 + self.inner - radius;
        }
        Spot {
            angle: self.angle(key),
            radius,
            depth,
        }
    }

    /// Spot of a fractional key: the nearest key's spot, nudged by the
    /// remainder (±½ semitone) along the dial. Chromatic order makes this
    /// continuous; in fifths each key gets its own ±½-step wedge. On a spiral
    /// the radius climbs with it; rings keep the key's ring.
    pub fn place_f(&self, m: f32) -> Spot {
        let key = m.round() as i32;
        let d = m - key as f32;
        let mut s = self.place(key);
        let step = TAU / 12.0 * d;
        s.angle += if self.clockwise { step } else { -step };
        s.depth += d / 12.0;
        if self.radial == Radial::Spiral {
            let dr = (1.0 - self.inner) / (self.octaves() as f32 - 1.0 / 12.0) * d / 12.0;
            s.radius += if self.invert { -dr } else { dr };
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock_hour(a: f32) -> f32 {
        (a / TAU * 12.0).rem_euclid(12.0)
    }

    #[test]
    fn c_at_north() {
        let l = Layout::default();
        assert_eq!(l.place(60).angle, 0.0);
        assert!((clock_hour(l.place(63).angle) - 3.0).abs() < 1e-4);
    }

    #[test]
    fn fifths_put_g_at_one() {
        let l = Layout {
            fifths: true,
            ..Default::default()
        };
        assert!((clock_hour(l.place(67).angle) - 1.0).abs() < 1e-4);
        assert!((clock_hour(l.place(65).angle) - 11.0).abs() < 1e-4); // F
    }

    #[test]
    fn offset_and_direction() {
        let l = Layout {
            offset: 9,
            clockwise: false,
            ..Default::default()
        };
        assert_eq!(l.place(57).angle, 0.0); // A at north
        assert!((clock_hour(l.place(59).angle) - 10.0).abs() < 1e-4); // B two steps back
    }

    #[test]
    fn spiral_grows() {
        for fifths in [false, true] {
            let l = Layout {
                radial: Radial::Spiral,
                fifths,
                lo: 30,
                hi: 70,
                offset: 4,
                ..Default::default()
            };
            let r: Vec<f32> = (l.lo..=l.hi).map(|k| l.place(k).radius).collect();
            assert!(r.windows(2).all(|w| w[1] > w[0]));
            assert!(r[0] >= l.inner && *r.last().unwrap() <= 1.0);
        }
    }

    #[test]
    fn rings_by_octave() {
        let l = Layout {
            radial: Radial::Rings,
            lo: 36,
            hi: 71,
            ..Default::default()
        };
        assert_eq!(l.octaves(), 3);
        assert_eq!(l.place(36).radius, l.inner);
        assert_eq!(l.place(47).radius, l.inner);
        assert_eq!(l.place(71).radius, 1.0);
    }

    #[test]
    fn fractional_keys_are_continuous() {
        for radial in [Radial::Single, Radial::Spiral] {
            let l = Layout { radial, ..Default::default() };
            for k in l.lo..l.hi {
                let (a, b) = (l.place_f(k as f32 + 0.4999), l.place_f(k as f32 + 0.5001));
                let da = (a.angle - b.angle).rem_euclid(TAU);
                assert!(da.min(TAU - da) < 1e-3);
                assert!((a.radius - b.radius).abs() < 1e-3);
                assert_eq!(l.place_f(k as f32), l.place(k));
            }
        }
    }

    #[test]
    fn invert_mirrors() {
        for radial in [Radial::Rings, Radial::Spiral] {
            let l = Layout {
                radial,
                ..Default::default()
            };
            let inv = Layout { invert: true, ..l };
            for k in l.lo..=l.hi {
                let (a, b) = (l.place(k), inv.place(k));
                assert!((a.radius + b.radius - (1.0 + l.inner)).abs() < 1e-5);
                assert_eq!(a.angle, b.angle);
            }
        }
    }
}

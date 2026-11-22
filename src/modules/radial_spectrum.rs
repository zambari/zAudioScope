//! Audio spectrum wrapped round the Note Clock dial, so peaks land on the
//! note lamps.
//!
//! - [`Wrap::Octave`]: one octave from the range's low note to +12, round a
//!   single circle (the bass-channel visualiser look).
//! - [`Wrap::Octaves`]: the whole range, one pass per octave, following the
//!   dial's layout: overlaid on a single ring, a ring each, or one long
//!   strip up the spiral.
//!
//! One FFT per new audio block on a decimated copy of the input (decimated
//! as far as the top note allows, so low notes get fine bins), mapped to
//! points per semitone through a cached [`Bands`] table. The display eases
//! fast up and slower down. Fills blend additively so overlaid octaves glow.

use egui::{Color32, Mesh, Pos2, Shape, Stroke, Ui, vec2};
use serde::{Deserialize, Serialize};

use super::Shared;
use super::note_input::ease;
use crate::audio::spectrum::{Bands, FLOOR_DB};
use crate::pitch;
use crate::ui::dynamic_range::DynamicRange;
use crate::ui::noterange::A4;
use crate::ui::polar::{Layout, Radial, Spot};
use crate::ui::theme;
use crate::ui::widgets::{self, Foldout};

/// The decimated rate stays at least this many times the top frequency
/// (the half-band filter is gentle near Nyquist).
const OVERSAMPLE: f32 = 6.0;
/// Most the input is decimated, as a power of two.
const MAX_FACTOR: u32 = 64;
/// Rise time of the display, s.
const ATTACK_S: f32 = 0.02;
const SIZES: [usize; 4] = [2048, 4096, 8192, 16384];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wrap {
    Octave,
    Octaves,
}

impl Wrap {
    const ALL: [Wrap; 2] = [Wrap::Octave, Wrap::Octaves];
    fn label(self) -> &'static str {
        match self {
            Wrap::Octave => "one octave",
            Wrap::Octaves => "every octave",
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    pub enabled: bool,
    wrap: Wrap,
    /// Peak height at full level, × the ring spacing.
    scale: f32,
    opacity: f32,
    /// Peaks grow towards the centre.
    inward: bool,
    db: DynamicRange,
    /// Fall time of the display, s.
    release_s: f32,
    /// FFT size, decimated samples.
    size: usize,
    /// Points per semitone.
    steps: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            enabled: false,
            wrap: Wrap::Octave,
            scale: 1.0,
            opacity: 0.8,
            inward: false,
            db: DynamicRange::default(),
            release_s: 0.25,
            size: 8192,
            steps: 8,
        }
    }
}

impl Params {
    pub fn sanitize(&mut self) {
        self.scale = self.scale.clamp(0.1, 4.0);
        self.opacity = self.opacity.clamp(0.0, 1.0);
        self.release_s = self.release_s.clamp(0.02, 3.0);
        if !SIZES.contains(&self.size) {
            self.size = 8192;
        }
        self.steps = self.steps.clamp(1, 16);
        self.db.sanitize();
    }

    /// The layout drawn on: one octave always goes round a single circle.
    fn geometry(&self, l: &Layout) -> Layout {
        match self.wrap {
            Wrap::Octave => Layout {
                radial: Radial::Single,
                ..*l
            },
            Wrap::Octaves => *l,
        }
    }
}

/// A run of points drawn as one strip: keys `from..=to` (fractional), at a
/// fixed radius (a ring) or the layout's own (spiral).
struct Pass {
    from: f32,
    to: f32,
    radius: Option<f32>,
}

fn passes(p: &Params, l: &Layout) -> Vec<Pass> {
    let lo = l.lo as f32;
    match (p.wrap, l.radial) {
        (Wrap::Octave, _) => vec![Pass {
            from: lo,
            to: lo + 12.0,
            radius: Some(1.0),
        }],
        (Wrap::Octaves, Radial::Spiral) => {
            vec![Pass {
                from: lo - 0.5,
                to: l.hi as f32 + 0.5,
                radius: None,
            }]
        }
        // Whole turns, seam at north, each on its octave's ring.
        (Wrap::Octaves, _) => {
            let first = l.offset + 12 * (l.lo - l.offset).div_euclid(12);
            (first..=l.hi)
                .step_by(12)
                .map(|k| Pass {
                    from: k as f32,
                    to: k as f32 + 12.0,
                    radius: Some(l.place(k).radius),
                })
                .collect()
        }
    }
}

/// What the cached band table was built for.
#[derive(Clone, Copy, PartialEq)]
struct Key {
    n: usize,
    rate: u32,
    m0: f32,
    count: usize,
    steps: usize,
}

#[derive(Default)]
pub struct RadialSpectrum {
    bands: Option<(Key, Bands)>,
    /// Latest analysis and its eased display copy, dB per point; point `j`
    /// is key `m0 + j / steps`.
    target: Vec<f32>,
    shown: Vec<f32>,
    last_end: u64,
    /// Samples of input wanted per frame (see `Module::lookback`).
    pub need: usize,
    /// (Hz per bin, window ms) of the last analysis, for the controls.
    info: (f32, f32),
}

impl RadialSpectrum {
    /// Analyse new audio (if any) and ease the display by `dt` seconds.
    pub fn update(&mut self, p: &mut Params, l: &Layout, shared: &Shared, dt: f32) {
        p.db.sync(&shared.db);
        let passes = passes(p, l);
        let m0 = passes.iter().map(|x| x.from).fold(f32::MAX, f32::min);
        let m1 = passes.iter().map(|x| x.to).fold(f32::MIN, f32::max);
        let steps = p.steps;
        let count = ((m1 - m0) * steps as f32).round() as usize + 1;
        let rate = shared.frame.rate.max(1);
        let f_top = pitch::midi_to_freq(m1 + 0.5, A4);
        let mut factor = 1u32;
        while factor < MAX_FACTOR && (rate / (factor * 2)) as f32 >= f_top * OVERSAMPLE {
            factor *= 2;
        }
        let n = p.size;
        self.need = (n + 8) * factor as usize;
        let key = Key {
            n,
            rate: rate / factor,
            m0,
            count,
            steps,
        };
        let fresh = self.bands.as_ref().is_none_or(|(k, _)| *k != key);
        if fresh {
            let h = 0.5 / steps as f32;
            let span = (
                pitch::midi_to_freq(m0 - h, A4),
                pitch::midi_to_freq(m0 + (count as f32 - 0.5) / steps as f32, A4),
            );
            self.bands = Some((key, Bands::new(n, key.rate, count, span)));
            self.info = (
                key.rate as f32 / n as f32,
                n as f32 / key.rate as f32 * 1000.0,
            );
            self.target = vec![FLOOR_DB; count];
            self.shown = self.target.clone();
        }
        let end = shared.frame.end();
        if fresh || end != self.last_end {
            self.last_end = end;
            let (_, bands) = self.bands.as_ref().unwrap();
            self.target = bands.db(&shared.decimated_spectrum(factor, n));
        }
        let (up, down) = (ease(dt, ATTACK_S), ease(dt, p.release_s));
        for (d, &t) in self.shown.iter_mut().zip(&self.target) {
            *d += (t - *d) * if t > *d { up } else { down };
        }
    }

    /// Strips from each pass's ring (or spiral) outward (or inward) by level,
    /// coloured by `color` of the nearest key. Face of `r` px round `c`.
    pub fn paint(
        &self,
        painter: &egui::Painter,
        p: &Params,
        l: &Layout,
        c: Pos2,
        r: f32,
        color: impl Fn(i32, &Spot) -> Color32,
    ) {
        let Some((key, _)) = &self.bands else { return };
        if self.shown.len() != key.count {
            return;
        }
        let g = p.geometry(l);
        let height = p.scale * g.spacing() * r * if p.inward { -1.0 } else { 1.0 };
        let steps = key.steps as f32;
        let mut mesh = Mesh::default();
        let mut strip: Vec<(Pos2, Pos2, Color32, f32)> = Vec::new();
        let flush = |strip: &mut Vec<(Pos2, Pos2, Color32, f32)>, mesh: &mut Mesh| {
            if strip.len() >= 2 {
                let first = mesh.vertices.len() as u32;
                for &(base, tip, col, v) in strip.iter() {
                    mesh.colored_vertex(base, theme::additive(col, p.opacity * 0.08));
                    mesh.colored_vertex(tip, theme::additive(col, p.opacity * (0.25 + 0.75 * v)));
                }
                for i in 0..strip.len() as u32 - 1 {
                    let v = first + 2 * i;
                    mesh.add_triangle(v, v + 1, v + 2);
                    mesh.add_triangle(v + 1, v + 3, v + 2);
                }
                let col = strip[strip.len() / 2].2;
                painter.add(Shape::line(
                    strip.iter().map(|s| s.1).collect(),
                    Stroke::new(1.2, col.gamma_multiply(p.opacity)),
                ));
            }
            strip.clear();
        };
        for pass in passes(p, l) {
            let a = ((pass.from - key.m0) * steps).round() as usize;
            let b = (((pass.to - key.m0) * steps).round() as usize).min(key.count - 1);
            let mut last_key = None;
            for j in a..=b {
                let m = key.m0 + j as f32 / steps;
                let k = m.round() as i32;
                // Fifths: each key is its own wedge, so break between keys.
                if l.fifths && last_key.is_some_and(|x| x != k) {
                    flush(&mut strip, &mut mesh);
                }
                last_key = Some(k);
                let spot = g.place_f(m);
                let v = p.db.level(self.shown[j]);
                let rad = pass.radius.unwrap_or(spot.radius) * r;
                let dir = vec2(spot.angle.sin(), -spot.angle.cos());
                let tip = (rad + height * v).max(0.0);
                strip.push((c + dir * rad, c + dir * tip, color(k, &spot), v));
            }
            flush(&mut strip, &mut mesh);
        }
        painter.add(Shape::mesh(mesh));
    }

    /// The "spectrum" foldout.
    pub fn controls(&self, p: &mut Params, ui: &mut Ui, shared: &Shared) {
        let summary = if p.enabled { p.wrap.label() } else { "off" };
        Foldout::new("note_clock_spectrum", "audio spectrum")
            .summary(summary)
            .show(ui, |ui| {
                ui.checkbox(&mut p.enabled, "show")
                    .on_hover_text("Wrap the input's spectrum round the dial");
                ui.add_enabled_ui(p.enabled, |ui| {
                    widgets::choice(ui, "wrap", &mut p.wrap, &Wrap::ALL, |w| w.label().into());
                    ui.add(
                        egui::Slider::new(&mut p.scale, 0.1..=4.0)
                            .logarithmic(true)
                            .prefix("×")
                            .text("radial scale"),
                    )
                    .on_hover_text("Peak height at full level, in ring spacings");
                    ui.add(egui::Slider::new(&mut p.opacity, 0.0..=1.0).text("opacity"));
                    ui.checkbox(&mut p.inward, "grow inward");
                    p.db.ui(ui, shared);
                    ui.add(
                        egui::Slider::new(&mut p.release_s, 0.02..=3.0)
                            .logarithmic(true)
                            .suffix(" s")
                            .text("release"),
                    );
                    let (hz, ms) = self.info;
                    widgets::choice(ui, "FFT size", &mut p.size, &SIZES, |n| n.to_string());
                    ui.weak(format!("{hz:.2} Hz per bin · {ms:.0} ms window"))
                        .on_hover_text("Decimated as far as the top note allows; bigger sizes resolve low notes better but respond slower");
                    ui.add(egui::Slider::new(&mut p.steps, 1..=16).text("pts/semi"));
                });
            });
        p.sanitize();
    }
}

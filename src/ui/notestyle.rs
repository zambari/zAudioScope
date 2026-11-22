//! How a note is drawn on a time-scrolling roll: lane width, an attack
//! fade-in from the note-on, a release tail after the note-off, and faint
//! overtone "ghosts" (a subharmonic and two harmonics) in other lanes.
//!
//! Everything goes into one [`Mesh`] of per-vertex-coloured quads (one quad
//! per envelope segment), so a few thousand notes cost a single draw call.
//! The module keeps one style for now; it is plain data, so per-channel
//! styles later are just `[NoteStyle; 16]`.

use egui::{Color32, Mesh, Rangef, Ui, pos2};
use serde::{Deserialize, Serialize};

use super::theme;
use super::widgets::Foldout;

/// Level at the very note-on when there is an attack fade, so the start
/// still reads.
const ATTACK_FLOOR: f32 = 0.2;

/// Overtone lanes relative to the note: an octave below, an octave above
/// (2nd harmonic) and an octave + fifth above (3rd harmonic, 19.02 st).
pub const OVERTONES: [(i32, &str); 3] = [(-12, "sub"), (12, "2nd"), (19, "3rd")];
/// Ghosts are narrower than the note itself.
const GHOST_WIDTH: f32 = 0.6;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NoteStyle {
    /// Fraction of the lane the note fills, centred.
    pub width: f32,
    /// Fade-in after the note-on.
    pub attack_s: f32,
    /// Tail after the note-off, fading to nothing.
    pub release_s: f32,
    /// How much velocity dims soft notes (0 = all equal).
    pub velocity: f32,
    /// Ghost strength 0..1 for each of [`OVERTONES`].
    pub overtones: [f32; 3],
    /// Opacity of the thin line marking the exact note-on / note-off (0 = hidden).
    pub edges: f32,
    /// Hue of the notes, -1..1: 0 is the accent orange, ±1 is half a turn
    /// round the wheel (the same range the impulse module's hue offset uses).
    pub hue: f32,
}

impl Default for NoteStyle {
    fn default() -> Self {
        Self {
            width: 0.55,
            attack_s: 0.0,
            release_s: 0.15,
            velocity: 0.6,
            overtones: [0.0; 3],
            edges: 0.5,
            hue: 0.0,
        }
    }
}

/// A piecewise-linear envelope: (time, level) breakpoints.
type Env = ([(f64, f32); 4], usize);

impl NoteStyle {
    pub fn sanitize(&mut self) {
        self.width = self.width.clamp(0.1, 1.0);
        self.attack_s = self.attack_s.clamp(0.0, 2.0);
        self.release_s = self.release_s.clamp(0.0, 4.0);
        self.velocity = self.velocity.clamp(0.0, 1.0);
        self.edges = self.edges.clamp(0.0, 1.0);
        self.hue = self.hue.clamp(-1.0, 1.0);
        for g in &mut self.overtones {
            *g = g.clamp(0.0, 1.0);
        }
    }

    /// Brightness for a velocity 0..127.
    pub fn vel_gain(&self, vel: u8) -> f32 {
        1.0 - self.velocity * (1.0 - vel as f32 / 127.0)
    }

    /// The parameter-column editor, folded.
    pub fn ui(&mut self, ui: &mut Ui) {
        let summary = format!(
            "A {:.0} / R {:.0} ms",
            self.attack_s * 1e3,
            self.release_s * 1e3
        );
        Foldout::new("note_style", "note look").summary(summary).show(ui, |ui| {
            ui.add(egui::Slider::new(&mut self.width, 0.1..=1.0).text("width"));
            let ms = |v: &mut f32, max: f32, label: &str, ui: &mut Ui| {
                let mut m = *v * 1e3;
                if ui.add(egui::Slider::new(&mut m, 0.0..=max).logarithmic(true).suffix(" ms").text(label)).changed() {
                    *v = m / 1e3;
                }
            };
            ms(&mut self.attack_s, 2000.0, "attack", ui);
            ms(&mut self.release_s, 4000.0, "release", ui);
            ui.add(egui::Slider::new(&mut self.velocity, 0.0..=1.0).text("velocity"));
            ui.add(egui::Slider::new(&mut self.edges, 0.0..=1.0).text("on/off edges"));
            ui.add(egui::Slider::new(&mut self.hue, -1.0..=1.0).text("hue"))
                .on_hover_text("Note colour: 0 is the accent orange; ±1 turns it half way round the colour wheel");
        });
        let on = self.overtones.iter().filter(|&&g| g > 0.0).count();
        Foldout::new("note_overtones", "overtones")
            .summary(if on == 0 {
                "off".into()
            } else {
                format!("{on} on")
            })
            .show(ui, |ui| {
                for ((_, name), g) in OVERTONES.iter().zip(&mut self.overtones) {
                    ui.add(egui::Slider::new(g, 0.0..=1.0).text(*name));
                }
            });
        self.sanitize();
    }

    /// The note colour for a palette colour, rotated by [`Self::hue`].
    pub fn tint(&self, c: Color32) -> Color32 {
        theme::hue_shift(c, self.hue * 0.5)
    }

    /// Seconds a note stays visible after its note-off.
    pub fn tail_s(&self) -> f64 {
        self.release_s as f64
    }

    /// Horizontal extent inside a lane `[left, left + lane_w]`.
    pub fn span(&self, left: f32, lane_w: f32, ghost: bool) -> Rangef {
        let w = lane_w * self.width * if ghost { GHOST_WIDTH } else { 1.0 };
        let c = left + lane_w / 2.0;
        Rangef::new(c - w / 2.0, c + w / 2.0)
    }

    /// Breakpoints from the note-on to where the tail dies, cut at `limit`.
    /// `off` is `None` while held; the hold lasts at least `min_hold`.
    fn envelope(&self, on: f64, off: Option<f64>, now: f64, min_hold: f64) -> Env {
        let attack = self.attack_s as f64;
        let level = |t: f64| {
            if attack <= 0.0 {
                1.0
            } else {
                (ATTACK_FLOOR + (1.0 - ATTACK_FLOOR) * ((t - on) / attack) as f32).min(1.0)
            }
        };
        let hold_end = off.unwrap_or(now).max(on + min_hold);
        let limit = now.max(on + min_hold);
        let mut pts = [(0.0, 0.0); 4];
        let mut n = 0;
        let mut push = |p: (f64, f32)| {
            pts[n] = p;
            n += 1;
        };
        push((on, level(on)));
        if on + attack < hold_end {
            push((on + attack, 1.0));
        }
        let at_off = level(hold_end);
        push((hold_end, at_off));
        if off.is_some() && self.release_s > 0.0 {
            let end = hold_end + self.release_s as f64;
            if end <= limit {
                push((end, 0.0));
            } else {
                let u = ((limit - hold_end) / self.release_s as f64) as f32;
                push((limit, at_off * (1.0 - u)));
            }
        }
        (pts, n)
    }

    /// Append one note (or ghost) to `mesh`: a quad per envelope segment,
    /// `color` scaled by the envelope and `gain`. `y` maps time to pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn add_note(
        &self,
        mesh: &mut Mesh,
        x: Rangef,
        color: Color32,
        gain: f32,
        (on, off): (f64, Option<f64>),
        now: f64,
        min_hold: f64,
        y: &impl Fn(f64) -> f32,
    ) {
        let (pts, n) = self.envelope(on, off, now, min_hold);
        let col = |l: f32| color.gamma_multiply(l * gain);
        for w in pts[..n].windows(2) {
            let ((t0, l0), (t1, l1)) = (w[0], w[1]);
            if t1 <= t0 {
                continue;
            }
            let (y0, y1) = (y(t0), y(t1));
            let i = mesh.vertices.len() as u32;
            mesh.colored_vertex(pos2(x.min, y0), col(l0));
            mesh.colored_vertex(pos2(x.max, y0), col(l0));
            mesh.colored_vertex(pos2(x.max, y1), col(l1));
            mesh.colored_vertex(pos2(x.min, y1), col(l1));
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(s: &NoteStyle, on: f64, off: Option<f64>, now: f64) -> Vec<(f64, f32)> {
        let (p, n) = s.envelope(on, off, now, 0.0);
        p[..n].to_vec()
    }

    #[test]
    fn released_note_fades_out() {
        let s = NoteStyle {
            attack_s: 0.1,
            release_s: 0.2,
            ..Default::default()
        };
        let e = env(&s, 0.0, Some(0.5), 10.0);
        assert_eq!(e.len(), 4);
        assert!((e[1].0 - 0.1).abs() < 1e-6 && e[1].1 == 1.0);
        assert_eq!(e[3].1, 0.0);
        // Tail cut at now: halfway down.
        let e = env(&s, 0.0, Some(0.5), 0.6);
        assert!((e[3].1 - 0.5).abs() < 1e-4);
    }

    #[test]
    fn hue_zero_keeps_accent_and_ends_meet_opposite() {
        let orange = theme::ACCENT;
        assert_eq!(NoteStyle::default().tint(orange), orange);
        // ±1 is half a turn: the same colour from either end.
        let (lo, hi) = (
            NoteStyle {
                hue: -1.0,
                ..Default::default()
            },
            NoteStyle {
                hue: 1.0,
                ..Default::default()
            },
        );
        assert_eq!(lo.tint(orange), hi.tint(orange));
        assert_ne!(hi.tint(orange), orange);
    }

    #[test]
    fn short_note_inside_attack() {
        let s = NoteStyle {
            attack_s: 1.0,
            release_s: 0.0,
            ..Default::default()
        };
        let e = env(&s, 0.0, Some(0.5), 10.0);
        assert_eq!(e.len(), 2);
        assert!(e[1].1 < 1.0 && e[1].1 > ATTACK_FLOOR);
    }
}

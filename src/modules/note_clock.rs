//! Note Clock: incoming MIDI notes lit on a polar dial, like an LED clock.
//! Pitch classes go round (chromatic or circle of fifths, any note at
//! north, either direction); octaves fold onto one ring, get a ring each,
//! or climb a spiral (see [`polar`]). Keys glow with [`KeyGlow`]: held keys
//! bright, released ones fading, often-played ones keeping a warm glow.
//!
//! One list of `(spot, level, held, colour)` feeds every style (LEDs,
//! segments, ticks, lines), so switching styles is instant. Lines (a chord
//! polygon, or radial spokes) fade in and out with their own attack/release. The
//! appearance lives in a "Controls" popup on the display (and in the
//! inspector); everything is persisted.

use std::f32::consts::TAU;

use egui::{
    Align, Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, TextureHandle, Ui, UiBuilder,
    pos2, vec2,
};
use serde::{Deserialize, Serialize};

use super::radial_spectrum::{self, RadialSpectrum};
use super::{Module, Shared};
use crate::pitch;
use crate::ui::keyglow::{KeyGlow, Timing};
use crate::ui::polar::{self, Radial, Spot};
use crate::ui::theme;
use crate::ui::widgets;

pub const TITLE: &str = "Note Clock";
/// Height of the strip holding the Controls button.
const BAR_H: f32 = 22.0;
/// Seconds per turn at full auto-rotation.
const SPIN_PERIOD_S: f32 = 12.0;
/// Link end standing for the dial's centre (radial lines).
const CENTER: i32 = -1;
/// Computer keys that play the chromatic octave on this page, C upwards:
/// Z S X D C V G B H N J M, as on a piano keyboard.
const KEYBOARD: [egui::Key; 12] = [
    egui::Key::Z,
    egui::Key::S,
    egui::Key::X,
    egui::Key::D,
    egui::Key::C,
    egui::Key::V,
    egui::Key::G,
    egui::Key::B,
    egui::Key::H,
    egui::Key::N,
    egui::Key::J,
    egui::Key::M,
];

/// One line between two notes (or the centre and a note), fading in while
/// its notes are held and out after.
struct Link {
    /// Shown keys, `a < b`; `a` may be [`CENTER`].
    a: i32,
    b: i32,
    on: bool,
    level: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Style {
    Leds,
    Segments,
    Ticks,
    Chord,
}

impl Style {
    const ALL: [Style; 4] = [Style::Leds, Style::Segments, Style::Ticks, Style::Chord];
    fn label(self) -> &'static str {
        match self {
            Style::Leds => "LEDs",
            Style::Segments => "segments",
            Style::Ticks => "ticks",
            Style::Chord => "lines only",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Hue {
    Single,
    PitchClass,
    Octave,
}

impl Hue {
    const ALL: [Hue; 3] = [Hue::Single, Hue::PitchClass, Hue::Octave];
    fn label(self) -> &'static str {
        match self {
            Hue::Single => "one colour",
            Hue::PitchClass => "hue by note",
            Hue::Octave => "hue by octave",
        }
    }
}

/// How the held notes are joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Lines {
    /// Neighbours round the dial: a chord becomes an outline.
    Polygon,
    /// As `Polygon`, with the shape filled (single ring only).
    Filled,
    /// A spoke from the centre to each note.
    Radial,
}

impl Lines {
    const ALL: [Lines; 3] = [Lines::Polygon, Lines::Filled, Lines::Radial];
    fn label(self) -> &'static str {
        match self {
            Lines::Polygon => "polygon",
            Lines::Filled => "filled polygon",
            Lines::Radial => "radial",
        }
    }
}

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    style: Style,
    /// Ticks under the main style.
    ticks: bool,
    /// Lines joining the held notes, over the main style.
    chord: bool,
    lines: Lines,
    /// Line fade-in / fade-out time constants, s.
    line_attack_s: f32,
    line_release_s: f32,
    radial: Radial,
    invert: bool,
    fifths: bool,
    /// Pitch class at north, semitones from C, -6..=6.
    offset: i32,
    /// Auto-rotation, -2..=2; signed square sets the speed, 2 ≈ a turn per
    /// `SPIN_PERIOD_S`. Positive turns clockwise.
    spin: f32,
    clockwise: bool,
    /// Innermost ring radius, 0..1 of the face.
    inner: f32,
    led_size: f32,
    hue: Hue,
    labels: bool,
    /// Brightness of the unlit dots and guides, 0..1.
    skeleton: f32,
    /// Release time multiplier.
    release: f32,
    /// How long the heat lingers (cooling time constant), s.
    linger_s: f32,
    /// Source channel 0..16; `None` means all.
    channel: Option<u8>,
    /// Use the shared note range; otherwise `octaves` from C(`low_octave`).
    follow_range: bool,
    low_octave: i32,
    octaves: i32,
    /// Pitch classes ringed on the dial, C..B.
    marked: [bool; 12],
    /// Octave the computer keys start from (C of this octave, MIDI 12·(n+1)).
    keys_octave: i32,
    /// Audio spectrum wrapped round the dial.
    spectrum: radial_spectrum::Params,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            style: Style::Leds,
            ticks: false,
            chord: false,
            lines: Lines::Filled,
            line_attack_s: 0.03,
            line_release_s: 0.6,
            radial: Radial::Single,
            invert: false,
            fifths: false,
            offset: 0,
            spin: 0.0,
            clockwise: true,
            inner: 0.35,
            led_size: 1.0,
            hue: Hue::PitchClass,
            labels: true,
            skeleton: 0.5,
            release: 1.0,
            linger_s: 20.0,
            channel: None,
            follow_range: true,
            low_octave: 2,
            octaves: 3,
            marked: [false; 12],
            keys_octave: 4,
            spectrum: Default::default(),
        }
    }
}

impl Params {
    fn sanitize(&mut self) {
        // Older saves held 0..12.
        self.offset = (self.offset + 6).rem_euclid(12) - 6;
        self.spin = self.spin.clamp(-2.0, 2.0);
        self.line_attack_s = self.line_attack_s.clamp(0.005, 2.0);
        self.line_release_s = self.line_release_s.clamp(0.02, 8.0);
        self.inner = self.inner.clamp(0.0, 0.9);
        self.led_size = self.led_size.clamp(0.3, 3.0);
        self.skeleton = self.skeleton.clamp(0.0, 1.0);
        self.release = self.release.clamp(0.2, 8.0);
        self.linger_s = self.linger_s.clamp(1.0, 120.0);
        self.channel = self.channel.filter(|&c| c < 16);
        self.low_octave = self.low_octave.clamp(-1, 8);
        self.octaves = self.octaves.clamp(1, 8);
        self.keys_octave = self.keys_octave.clamp(-1, 8);
        self.spectrum.sanitize();
    }
}

/// One key as drawn.
struct Lamp {
    key: i32,
    spot: Spot,
    pos: Pos2,
    level: f32,
    held: bool,
    /// Unlit and lit colours; draw `lerp(off, on, level)`.
    off: Color32,
    on: Color32,
}

impl Lamp {
    fn color(&self) -> Color32 {
        theme::lerp(self.off, self.on, self.level)
    }
    fn dir(&self) -> egui::Vec2 {
        vec2(self.spot.angle.sin(), -self.spot.angle.cos())
    }
}

#[derive(Default)]
pub struct NoteClock {
    p: Params,
    glow: KeyGlow,
    /// Radial-gradient sprite for the LED halos, made on first draw.
    halo: Option<TextureHandle>,
    lit: bool,
    connected: bool,
    missing: bool,
    /// Auto-rotation so far, radians.
    turn: f32,
    /// When the fades and rotation were last advanced, MIDI clock.
    last_t: Option<f64>,
    links: Vec<Link>,
    spectrum: RadialSpectrum,
    /// The note each computer key is sounding, by [`KEYBOARD`] slot.
    keys_down: [Option<u8>; 12],
}

impl NoteClock {
    /// Computer keys play notes while this page is on screen: a key down
    /// sounds its note into the same glow as MIDI, and lifting it (or losing
    /// focus, or typing into a field) ends it. Tracks key state rather than
    /// events, so a page switch mid-hold cannot leave a note stuck.
    fn play_keys(&mut self, ui: &Ui) {
        let (focused, mods, down) =
            ui.input(|i| (i.focused, i.modifiers, KEYBOARD.map(|k| i.key_down(k))));
        let playable =
            focused && !ui.ctx().text_edit_focused() && !(mods.command || mods.ctrl || mods.alt);
        let base = 12 * (self.p.keys_octave + 1);
        let t = crate::midi::now();
        for (slot, &pressed) in down.iter().enumerate() {
            match (self.keys_down[slot], playable && pressed) {
                (None, true) => {
                    let note = (base + slot as i32) as u8;
                    self.keys_down[slot] = Some(note);
                    self.glow.note_on(note, t);
                }
                (Some(note), false) => {
                    self.keys_down[slot] = None;
                    self.glow.note_off(note, t);
                }
                _ => {}
            }
        }
    }

    /// Advance the auto-rotation; it returns to rest when the speed is 0.
    fn spin(&mut self, dt: f32) {
        let s = self.p.spin;
        self.turn = if s == 0.0 {
            0.0
        } else {
            let turns_per_s = s * s.abs() / 4.0 / SPIN_PERIOD_S;
            (self.turn + turns_per_s * TAU * dt).rem_euclid(TAU)
        };
    }

    fn layout(&self, shared: &Shared) -> polar::Layout {
        let p = &self.p;
        let (lo, hi) = if p.follow_range {
            let r = shared.range.get();
            (r.lo, r.hi)
        } else {
            let lo = 12 * (p.low_octave + 1);
            (lo, lo + 12 * p.octaves - 1)
        };
        polar::Layout {
            fifths: p.fifths,
            offset: p.offset,
            clockwise: p.clockwise,
            radial: p.radial,
            invert: p.invert,
            lo: lo.clamp(0, 127),
            hi: hi.clamp(0, 127),
            inner: p.inner,
            turn: self.turn,
        }
    }

    /// Brightness and held state per shown key. Notes outside the range fold
    /// by octaves onto its nearest key of the same pitch class, so nothing
    /// played goes unseen.
    fn folded(&self, l: &polar::Layout) -> ([f32; 128], [bool; 128]) {
        let mut levels = [0.0f32; 128];
        let mut helds = [false; 128];
        for k in 0..128 {
            if let Some(f) = fold(k, l.lo, l.hi) {
                let f = f as usize;
                levels[f] = levels[f].max(self.glow.level(k));
                helds[f] |= self.glow.held(k);
            }
        }
        (levels, helds)
    }

    /// Held keys, one per spot on the dial (a single ring folds octaves
    /// together), in order round it.
    fn held_points(&self, l: &polar::Layout) -> Vec<i32> {
        let (_, held) = self.folded(l);
        let top = if l.radial == Radial::Single {
            (l.lo + 11).min(l.hi)
        } else {
            l.hi
        };
        let mut keys: Vec<i32> = (l.lo..=l.hi)
            .filter(|&k| held[k as usize])
            .filter_map(|k| fold(k, l.lo, top))
            .collect();
        let at = |k: &i32| {
            let s = l.place(*k);
            (s.angle.rem_euclid(TAU), s.radius)
        };
        keys.sort_by(|a, b| {
            let (a, b) = (at(a), at(b));
            a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
        });
        keys.dedup();
        keys
    }

    /// Lines wanted now, as `(a, b)` with `a < b`.
    fn wanted_links(&self, l: &polar::Layout) -> Vec<(i32, i32)> {
        if !(self.p.chord || self.p.style == Style::Chord) {
            return Vec::new();
        }
        let keys = self.held_points(l);
        match self.p.lines {
            Lines::Radial => keys.iter().map(|&k| (CENTER, k)).collect(),
            Lines::Polygon | Lines::Filled => {
                let n = keys.len();
                let edges = match n {
                    0 | 1 => 0,
                    2 => 1,
                    _ => n,
                };
                (0..edges)
                    .map(|i| {
                        let (a, b) = (keys[i], keys[(i + 1) % n]);
                        (a.min(b), a.max(b))
                    })
                    .collect()
            }
        }
    }

    /// Fade lines in towards `want` and out of everything else.
    fn fade_links(&mut self, want: &[(i32, i32)], dt: f32) {
        for link in &mut self.links {
            link.on = want.contains(&(link.a, link.b));
        }
        for &(a, b) in want {
            if !self.links.iter().any(|x| (x.a, x.b) == (a, b)) {
                self.links.push(Link {
                    a,
                    b,
                    on: true,
                    level: 0.0,
                });
            }
        }
        let (attack, release) = (self.p.line_attack_s, self.p.line_release_s);
        for link in &mut self.links {
            let (target, tau) = if link.on {
                (1.0, attack)
            } else {
                (0.0, release)
            };
            link.level += (target - link.level) * (1.0 - (-dt / tau.max(1e-3)).exp());
        }
        self.links.retain(|x| x.on || x.level > 0.004);
    }

    /// The fading lines; a filled polygon on a single ring is also filled.
    fn draw_links(&self, painter: &egui::Painter, l: &polar::Layout, c: Pos2, r: f32) {
        let at = |k: i32| {
            if k == CENTER {
                c
            } else {
                c + l.place(k).offset(r)
            }
        };
        if self.p.lines == Lines::Filled && l.radial == Radial::Single {
            let pts: Vec<Pos2> = self.held_points(l).into_iter().map(at).collect();
            let level = self
                .links
                .iter()
                .filter(|x| x.on)
                .map(|x| x.level)
                .fold(1.0f32, f32::min);
            if pts.len() >= 3 {
                // Points on one circle sorted by angle form a convex polygon.
                let fill = theme::ACCENT.gamma_multiply(0.25 * level);
                painter.add(Shape::convex_polygon(pts, fill, Stroke::NONE));
            }
        }
        for link in &self.links {
            let color = if link.a == CENTER {
                let spot = l.place(link.b);
                theme::lerp(self.hue(l, link.b, &spot), theme::ON_ACCENT, 0.3)
            } else {
                theme::ON_ACCENT.gamma_multiply(0.85)
            };
            painter.line_segment(
                [at(link.a), at(link.b)],
                Stroke::new(2.0, color.gamma_multiply(link.level)),
            );
        }
    }

    fn hue(&self, l: &polar::Layout, key: i32, spot: &Spot) -> Color32 {
        let base = theme::ACCENT_BRIGHT;
        match self.p.hue {
            Hue::Single => base,
            Hue::PitchClass => theme::hue_shift(base, l.step(key) as f32 / 12.0),
            Hue::Octave => theme::hue_shift(base, spot.depth.floor() * 0.17),
        }
    }

    fn halo_texture(&mut self, ctx: &egui::Context) -> egui::TextureId {
        self.halo
            .get_or_insert_with(|| {
                const N: usize = 64;
                let half = N as f32 / 2.0;
                let rgba: Vec<u8> = (0..N * N)
                    .flat_map(|i| {
                        let x = (i % N) as f32 + 0.5 - half;
                        let y = (i / N) as f32 + 0.5 - half;
                        let d = (x * x + y * y).sqrt() / half;
                        let a = ((1.0 - d).max(0.0).powi(2) * 255.0) as u8;
                        [a; 4]
                    })
                    .collect();
                let image = egui::ColorImage::from_rgba_premultiplied([N, N], &rgba);
                ctx.load_texture("note_clock_halo", image, egui::TextureOptions::LINEAR)
            })
            .id()
    }

    /// Every setting, for both the inspector and the Controls popup.
    fn controls(&mut self, ui: &mut Ui, shared: &Shared) {
        let p = &mut self.p;
        widgets::choice(ui, "style", &mut p.style, &Style::ALL, |s| s.label().into());
        ui.horizontal(|ui| {
            ui.add_enabled(
                p.style != Style::Ticks,
                egui::Checkbox::new(&mut p.ticks, "ticks"),
            )
            .on_hover_text("Radial ticks under the main style");
            ui.add_enabled(
                p.style != Style::Chord,
                egui::Checkbox::new(&mut p.chord, "lines"),
            )
            .on_hover_text("Join the held notes with lines");
        });
        ui.add_enabled_ui(p.chord || p.style == Style::Chord, |ui| {
            widgets::choice(ui, "lines", &mut p.lines, &Lines::ALL, |x| x.label().into());
            ui.add(
                egui::Slider::new(&mut p.line_attack_s, 0.005..=2.0)
                    .logarithmic(true)
                    .suffix(" s")
                    .text("line attack"),
            )
            .on_hover_text("Fade-in time when notes join");
            ui.add(
                egui::Slider::new(&mut p.line_release_s, 0.02..=8.0)
                    .logarithmic(true)
                    .suffix(" s")
                    .text("line release"),
            )
            .on_hover_text("Fade-out time after a note off breaks the line");
        });
        widgets::choice(ui, "layout", &mut p.radial, &Radial::ALL, |r| {
            r.label().into()
        });
        ui.add_enabled_ui(p.radial != Radial::Single, |ui| {
            ui.checkbox(&mut p.invert, "high notes inside");
            ui.add(egui::Slider::new(&mut p.inner, 0.0..=0.9).text("inner radius"));
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut p.fifths, "circle of fifths");
            let dir = if p.clockwise {
                "clockwise"
            } else {
                "counter-clockwise"
            };
            if ui
                .button(dir)
                .on_hover_text("Direction notes go round")
                .clicked()
            {
                p.clockwise = !p.clockwise;
            }
        });
        ui.add(
            egui::Slider::new(&mut p.offset, -6..=6)
                .custom_formatter(|v, _| {
                    format!(
                        "{v:+} {}",
                        pitch::NOTE_NAMES[(v as i32).rem_euclid(12) as usize]
                    )
                })
                .text("at north"),
        )
        .on_hover_text("Note at the top, in semitones from C");
        ui.add(egui::Slider::new(&mut p.spin, -2.0..=2.0).text("auto-rotate"))
            .on_hover_text("Rotation speed, squared; 0 stops and returns to rest");
        ui.add(egui::Slider::new(&mut p.led_size, 0.3..=3.0).text("LED size"));
        widgets::choice(ui, "colour", &mut p.hue, &Hue::ALL, |h| h.label().into());
        ui.checkbox(&mut p.labels, "note names");
        ui.horizontal(|ui| {
            ui.label("mark");
            widgets::note_row(ui, 0, None, &mut p.marked, |_| true);
            widgets::clear_notes(ui, &mut p.marked);
        })
        .response
        .on_hover_text("Ring these notes on the dial");
        ui.horizontal(|ui| {
            ui.label("computer keys");
            ui.add(
                egui::DragValue::new(&mut p.keys_octave)
                    .range(-1..=8)
                    .prefix("from C"),
            )
            .on_hover_text("On this page Z S X D C V G B H N J M play C to B from this octave");
        });
        ui.add(egui::Slider::new(&mut p.skeleton, 0.0..=1.0).text("unlit dots"));
        ui.add(
            egui::Slider::new(&mut p.release, 0.2..=8.0)
                .logarithmic(true)
                .prefix("×")
                .text("release"),
        );
        ui.add(
            egui::Slider::new(&mut p.linger_s, 1.0..=120.0)
                .logarithmic(true)
                .suffix(" s")
                .text("heat linger"),
        );
        // No open port, nothing to filter: the choice is kept for when it returns.
        if shared.midi.connected {
            let channels: Vec<Option<u8>> =
                std::iter::once(None).chain((0..16).map(Some)).collect();
            widgets::choice(ui, "MIDI channel", &mut p.channel, &channels, |c| {
                c.map_or("all".into(), |c| format!("{}", c + 1))
            });
        }
        ui.checkbox(&mut p.follow_range, "follow the shared note range");
        if !p.follow_range {
            ui.horizontal(|ui| {
                ui.label("from");
                ui.add(
                    egui::DragValue::new(&mut p.low_octave)
                        .range(-1..=8)
                        .prefix("C"),
                );
                ui.add(
                    egui::DragValue::new(&mut p.octaves)
                        .range(1..=8)
                        .suffix(" oct"),
                );
            });
        }
        self.spectrum.controls(&mut p.spectrum, ui, shared);
        p.sanitize();
    }

    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        let l = self.layout(shared);
        if self.p.spectrum.enabled {
            // Analysed here rather than in `update`: only while on screen.
            let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
            self.spectrum.update(&mut self.p.spectrum, &l, shared, dt);
        }
        let p = &self.p;
        let margin = if p.labels { 18.0 } else { 6.0 };
        let face = (rect.width().min(rect.height()) * 0.5 - margin).max(12.0);
        let led = (p.led_size * face * 0.03).clamp(1.5, 40.0);
        let r = (face - led * 1.5).max(8.0);
        let c = rect.center();
        let spacing = l.spacing() * r;
        let bg = ui.visuals().extreme_bg_color;
        let off = theme::lerp(bg, Color32::from_gray(95), p.skeleton);
        let (levels, helds) = self.folded(&l);
        let lamps: Vec<Lamp> = (l.lo..=l.hi)
            .map(|key| {
                let spot = l.place(key);
                let (level, held) = (levels[key as usize], helds[key as usize]);
                let mut on = self.hue(&l, key, &spot);
                if held {
                    on = theme::lerp(on, theme::ON_ACCENT, 0.3);
                }
                Lamp {
                    key,
                    spot,
                    pos: c + spot.offset(r),
                    level,
                    held,
                    off,
                    on,
                }
            })
            .collect();
        let painter = ui.painter_at(rect);

        // Skeleton guides: ring circles, or the chromatic spiral's path.
        if p.skeleton > 0.0 {
            let guide = Stroke::new(1.0, theme::lerp(bg, off, 0.45));
            match p.radial {
                Radial::Single => {
                    painter.circle_stroke(c, r, guide);
                }
                Radial::Rings => {
                    let mut radii: Vec<f32> = lamps.iter().map(|m| m.spot.radius).collect();
                    radii.dedup();
                    for rad in radii {
                        painter.circle_stroke(c, rad * r, guide);
                    }
                }
                Radial::Spiral if !p.fifths => {
                    painter.add(Shape::line(lamps.iter().map(|m| m.pos).collect(), guide));
                }
                Radial::Spiral => {}
            }
        }

        if p.spectrum.enabled {
            self.spectrum
                .paint(&painter, &p.spectrum, &l, c, r, |k, s| self.hue(&l, k, s));
        }
        if p.ticks || p.style == Style::Ticks {
            let half = (spacing * 0.35).clamp(3.0, 14.0).max(led * 1.4);
            for m in &lamps {
                let d = m.dir() * half;
                painter.line_segment(
                    [m.pos - d, m.pos + d],
                    Stroke::new(1.0 + m.level, m.color()),
                );
            }
        }
        // Marked notes: a soft disc under the lamp, ringed.
        if p.marked.contains(&true) {
            let rad = (led * 2.2).max(7.0).max(if p.style == Style::Segments {
                spacing * 0.45
            } else {
                0.0
            });
            let ring = Stroke::new(1.5, theme::ON_ACCENT.gamma_multiply(0.35));
            for m in lamps
                .iter()
                .filter(|m| p.marked[m.key.rem_euclid(12) as usize])
            {
                painter.circle(m.pos, rad, theme::ON_ACCENT.gamma_multiply(0.08), ring);
            }
        }
        match p.style {
            Style::Leds => {
                let tex = self.halo_texture(ui.ctx());
                self.leds(&painter, &lamps, led, tex);
            }
            Style::Segments => segments(&painter, &lamps, c, r, spacing),
            Style::Ticks => {}
            Style::Chord => {
                for m in &lamps {
                    if p.skeleton > 0.0 || m.level > 0.0 {
                        painter.circle_filled(m.pos, led * 0.6, m.color());
                    }
                }
            }
        }
        self.draw_links(&painter, &l, c, r);
        let p = &self.p;

        if p.labels {
            let font = FontId::proportional(10.0);
            for pc in 0..12 {
                let a = l.angle(pc);
                let at = c + vec2(a.sin(), -a.cos()) * (r + led + 9.0);
                let held = lamps.iter().any(|m| m.held && m.key.rem_euclid(12) == pc);
                let color = if held {
                    theme::ON_ACCENT
                } else {
                    ui.visuals().weak_text_color()
                };
                painter.text(
                    at,
                    Align2::CENTER_CENTER,
                    pitch::NOTE_NAMES[pc as usize],
                    font.clone(),
                    color,
                );
            }
            if p.radial == Radial::Single {
                let held: Vec<String> = lamps
                    .iter()
                    .filter(|m| m.held)
                    .map(|m| pitch::note_name(m.key))
                    .collect();
                painter.text(
                    c,
                    Align2::CENTER_CENTER,
                    held.join(" "),
                    FontId::proportional(13.0),
                    theme::ON_ACCENT,
                );
            }
        }
        if !self.connected {
            let text = if self.missing {
                "no device"
            } else {
                "no MIDI input selected"
            };
            painter.text(
                pos2(c.x, rect.bottom() - 4.0),
                Align2::CENTER_BOTTOM,
                text,
                FontId::proportional(12.0),
                ui.visuals().weak_text_color(),
            );
        }
    }

    /// Dim dots for unlit keys, a core plus an additive halo for lit ones.
    fn leds(&self, painter: &egui::Painter, lamps: &[Lamp], led: f32, tex: egui::TextureId) {
        let mut halos = Mesh::with_texture(tex);
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        for m in lamps {
            if self.p.skeleton > 0.0 || m.level > 0.0 {
                painter.circle_filled(m.pos, led, m.color());
            }
            if m.level > 0.0 {
                let tint = theme::additive(m.on, m.level);
                halos.add_rect_with_uv(
                    Rect::from_center_size(m.pos, egui::Vec2::splat(led * 5.0)),
                    uv,
                    tint,
                );
            }
        }
        painter.add(Shape::mesh(halos));
    }
}

/// `key` shifted by whole octaves into `lo..=hi`, or `None` when the range
/// holds no key of that pitch class.
fn fold(key: i32, lo: i32, hi: i32) -> Option<i32> {
    let k = if key < lo {
        key + 12 * (lo - key + 11).div_euclid(12)
    } else if key > hi {
        key - 12 * (key - hi + 11).div_euclid(12)
    } else {
        key
    };
    (lo..=hi).contains(&k).then_some(k)
}

/// Ring slices: one wedge per key, a twelfth of the turn wide (with a gap),
/// as thick as most of the ring spacing. One mesh.
fn segments(painter: &egui::Painter, lamps: &[Lamp], c: Pos2, r: f32, spacing: f32) {
    const ARC: usize = 6;
    let half = TAU / 24.0 * 0.82;
    let band = (spacing * 0.42).max(2.0);
    let mut mesh = Mesh::default();
    for m in lamps {
        let rad = m.spot.radius * r;
        let (r0, r1) = ((rad - band).max(0.0), rad + band);
        let color = m.color();
        let first = mesh.vertices.len() as u32;
        for i in 0..=ARC {
            let a = m.spot.angle - half + 2.0 * half * i as f32 / ARC as f32;
            let d = vec2(a.sin(), -a.cos());
            mesh.colored_vertex(c + d * r0, color);
            mesh.colored_vertex(c + d * r1, color);
        }
        for i in 0..ARC as u32 {
            let v = first + 2 * i;
            mesh.add_triangle(v, v + 1, v + 2);
            mesh.add_triangle(v + 1, v + 3, v + 2);
        }
    }
    painter.add(Shape::mesh(mesh));
}

impl Module for NoteClock {
    fn title(&self) -> &'static str {
        TITLE
    }

    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str) {
        eframe::set_value(storage, key, &self.p);
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        if let Some(p) = eframe::get_value::<Params>(storage, key) {
            self.p = p;
            self.p.sanitize();
        }
    }

    fn lookback(&self, _rate: u32) -> usize {
        if self.p.spectrum.enabled {
            self.spectrum.need
        } else {
            0
        }
    }

    fn solo_only(&self) -> bool {
        true
    }

    fn uses_audio(&self) -> bool {
        false
    }

    fn always_live(&self) -> bool {
        // Note on/off arrive once; skipping them would leave notes stuck held.
        true
    }

    fn update(&mut self, shared: &mut Shared) {
        let midi = &shared.midi;
        (self.connected, self.missing) = (midi.connected, midi.missing);
        self.glow.timing = Timing {
            release: self.p.release,
            heat_tau_s: self.p.linger_s,
        };
        self.lit = self.glow.update(midi, self.p.channel);
        let now = crate::midi::now();
        let dt = self
            .last_t
            .map_or(0.0, |t| (now - t).clamp(0.0, 0.25) as f32);
        self.last_t = Some(now);
        self.spin(dt);
        let want = self.wanted_links(&self.layout(shared));
        self.fade_links(&want, dt);
        self.lit |= !self.links.is_empty();
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        self.controls(ui, shared);
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        if self.lit || self.p.spin != 0.0 || self.p.spectrum.enabled {
            ui.ctx().request_repaint();
        }
        self.play_keys(ui);
        self.paint(ui, rect, shared);
        let mut bar = ui.new_child(
            UiBuilder::new()
                .id_salt("note_clock_bar")
                .max_rect(rect.with_max_y(rect.top() + BAR_H).shrink2(vec2(4.0, 1.0)))
                .layout(egui::Layout::right_to_left(Align::Center)),
        );
        widgets::popup_button(&mut bar, "Controls", "Note clock appearance", |ui| {
            self.controls(ui, shared)
        });
    }
}

//! Note Clock: incoming MIDI notes lit on a polar dial, like an LED clock.
//! Pitch classes go round (chromatic or circle of fifths, any note at
//! north, either direction); octaves fold onto one ring, get a ring each,
//! or climb a spiral (see [`polar`]). Keys glow with [`KeyGlow`]: held keys
//! bright, released ones fading, often-played ones keeping a warm glow.
//!
//! One list of `(spot, level, held, colour)` feeds every style (LEDs,
//! segments, ticks, chord polygon), so switching styles is instant. The
//! appearance lives in a "Controls" popup on the display (and in the
//! inspector); everything is persisted.

use std::f32::consts::TAU;

use egui::{
    Align, Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, TextureHandle, Ui, UiBuilder,
    pos2, vec2,
};
use serde::{Deserialize, Serialize};

use super::{Module, Shared};
use crate::pitch;
use crate::ui::keyglow::{KeyGlow, Timing};
use crate::ui::polar::{self, Radial, Spot};
use crate::ui::theme;
use crate::ui::widgets;

pub const TITLE: &str = "Note Clock";
/// Height of the strip holding the Controls button.
const BAR_H: f32 = 22.0;

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
            Style::Chord => "chord polygon",
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

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    style: Style,
    /// Ticks under the main style.
    ticks: bool,
    /// Chord polygon over the main style.
    chord: bool,
    radial: Radial,
    invert: bool,
    fifths: bool,
    /// Pitch class at north.
    offset: i32,
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
}

impl Default for Params {
    fn default() -> Self {
        Self {
            style: Style::Leds,
            ticks: false,
            chord: false,
            radial: Radial::Single,
            invert: false,
            fifths: false,
            offset: 0,
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
        }
    }
}

impl Params {
    fn sanitize(&mut self) {
        self.offset = self.offset.rem_euclid(12);
        self.inner = self.inner.clamp(0.0, 0.9);
        self.led_size = self.led_size.clamp(0.3, 3.0);
        self.skeleton = self.skeleton.clamp(0.0, 1.0);
        self.release = self.release.clamp(0.2, 8.0);
        self.linger_s = self.linger_s.clamp(1.0, 120.0);
        self.channel = self.channel.filter(|&c| c < 16);
        self.low_octave = self.low_octave.clamp(-1, 8);
        self.octaves = self.octaves.clamp(1, 8);
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
}

impl NoteClock {
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
    fn controls(&mut self, ui: &mut Ui) {
        let p = &mut self.p;
        widgets::choice(ui, "style", &mut p.style, &Style::ALL, |s| s.label().into());
        ui.horizontal(|ui| {
            ui.add_enabled(p.style != Style::Ticks, egui::Checkbox::new(&mut p.ticks, "ticks"))
                .on_hover_text("Radial ticks under the main style");
            ui.add_enabled(p.style != Style::Chord, egui::Checkbox::new(&mut p.chord, "chord"))
                .on_hover_text("Join the held notes into a polygon");
        });
        widgets::choice(ui, "layout", &mut p.radial, &Radial::ALL, |r| r.label().into());
        ui.add_enabled_ui(p.radial != Radial::Single, |ui| {
            ui.checkbox(&mut p.invert, "high notes inside");
            ui.add(egui::Slider::new(&mut p.inner, 0.0..=0.9).text("inner radius"));
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut p.fifths, "circle of fifths");
            let dir = if p.clockwise { "clockwise" } else { "counter-clockwise" };
            if ui.button(dir).on_hover_text("Direction notes go round").clicked() {
                p.clockwise = !p.clockwise;
            }
        });
        let names = |pc: i32| pitch::NOTE_NAMES[pc as usize].to_owned();
        widgets::choice(ui, "at north", &mut p.offset, &(0..12).collect::<Vec<_>>(), names);
        ui.add(egui::Slider::new(&mut p.led_size, 0.3..=3.0).text("LED size"));
        widgets::choice(ui, "colour", &mut p.hue, &Hue::ALL, |h| h.label().into());
        ui.checkbox(&mut p.labels, "note names");
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
        let channels: Vec<Option<u8>> = std::iter::once(None).chain((0..16).map(Some)).collect();
        widgets::choice(ui, "MIDI channel", &mut p.channel, &channels, |c| {
            c.map_or("all".into(), |c| format!("{}", c + 1))
        });
        ui.checkbox(&mut p.follow_range, "follow the shared note range");
        if !p.follow_range {
            ui.horizontal(|ui| {
                ui.label("from");
                ui.add(egui::DragValue::new(&mut p.low_octave).range(-1..=8).prefix("C"));
                ui.add(egui::DragValue::new(&mut p.octaves).range(1..=8).suffix(" oct"));
            });
        }
        p.sanitize();
    }

    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        let l = self.layout(shared);
        let p = &self.p;
        let margin = if p.labels { 18.0 } else { 6.0 };
        let face = (rect.width().min(rect.height()) * 0.5 - margin).max(12.0);
        let led = (p.led_size * face * 0.03).clamp(1.5, 40.0);
        let r = (face - led * 1.5).max(8.0);
        let c = rect.center();
        let spacing = l.spacing() * r;
        let bg = ui.visuals().extreme_bg_color;
        let off = theme::lerp(bg, Color32::from_gray(95), p.skeleton);
        let lamps: Vec<Lamp> = (l.lo..=l.hi)
            .map(|key| {
                let spot = l.place(key);
                let (level, held) = (self.glow.level(key), self.glow.held(key));
                let mut on = self.hue(&l, key, &spot);
                if held {
                    on = theme::lerp(on, theme::ON_ACCENT, 0.3);
                }
                Lamp { key, spot, pos: c + spot.offset(r), level, held, off, on }
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
        let p = &self.p;
        if p.chord || p.style == Style::Chord {
            chord(&painter, &lamps, c, p.radial == Radial::Single);
        }

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
                painter.text(at, Align2::CENTER_CENTER, pitch::NOTE_NAMES[pc as usize], font.clone(), color);
            }
            if p.radial == Radial::Single {
                let held: Vec<String> = lamps
                    .iter()
                    .filter(|m| m.held)
                    .map(|m| pitch::note_name(m.key))
                    .collect();
                painter.text(c, Align2::CENTER_CENTER, held.join(" "), FontId::proportional(13.0), theme::ON_ACCENT);
            }
        }
        if !self.connected {
            let text = if self.missing { "no device" } else { "no MIDI input selected" };
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
                let [r, g, b, _] = m.on.gamma_multiply(m.level).to_array();
                // Zero alpha in premultiplied colour blends additively.
                let tint = Color32::from_rgba_premultiplied(r, g, b, 0);
                halos.add_rect_with_uv(Rect::from_center_size(m.pos, egui::Vec2::splat(led * 5.0)), uv, tint);
            }
        }
        painter.add(Shape::mesh(halos));
    }
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

/// Lines joining the held notes in order round the dial: intervals become
/// shapes (a major triad is the same triangle in any key).
fn chord(painter: &egui::Painter, lamps: &[Lamp], c: Pos2, fill: bool) {
    let mut pts: Vec<Pos2> = Vec::new();
    for m in lamps.iter().filter(|m| m.held) {
        if !pts.iter().any(|q| q.distance(m.pos) < 0.5) {
            pts.push(m.pos);
        }
    }
    let around = |q: &Pos2| (q.x - c.x).atan2(c.y - q.y).rem_euclid(TAU);
    pts.sort_by(|a, b| around(a).total_cmp(&around(b)));
    let stroke = Stroke::new(2.0, theme::ON_ACCENT.gamma_multiply(0.85));
    match pts.len() {
        0 | 1 => {}
        2 => {
            painter.line_segment([pts[0], pts[1]], stroke);
        }
        _ => {
            // Points on one circle sorted by angle form a convex polygon.
            if fill {
                painter.add(Shape::convex_polygon(pts.clone(), theme::ACCENT.gamma_multiply(0.25), Stroke::NONE));
            }
            painter.add(Shape::closed_line(pts, stroke));
        }
    }
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
        0
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
    }

    fn params(&mut self, ui: &mut Ui, _shared: &Shared) {
        self.controls(ui);
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        if self.lit {
            ui.ctx().request_repaint();
        }
        self.paint(ui, rect, shared);
        let mut bar = ui.new_child(
            UiBuilder::new()
                .id_salt("note_clock_bar")
                .max_rect(rect.with_max_y(rect.top() + BAR_H).shrink2(vec2(4.0, 1.0)))
                .layout(egui::Layout::right_to_left(Align::Center)),
        );
        widgets::popup_button(&mut bar, "Controls", "Note clock appearance", |ui| {
            self.controls(ui)
        });
    }
}

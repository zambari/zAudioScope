//! MIDI monitor: pick an input port, see which of the 16 channels are
//! active, pick one (or all) as the source, and watch incoming notes fall
//! down a piano-roll waterfall (newest at the top). The key range is the
//! shared note range (the bar above the status bar).
//!
//! Messages come from the app's [`MidiHub`](crate::midi::MidiHub), which
//! owns the port and keeps it alive while this module is hidden.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use egui::{Align2, Color32, FontId, Mesh, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};

use super::{Module, Shared};
use crate::pitch;
use crate::ui::icons::{self, Dir};
use crate::ui::noterange::{NoteRange, is_black};
use crate::ui::notestyle::{NoteStyle, OVERTONES};
use crate::ui::theme;

/// Notes kept in the ring.
const RING: usize = 4096;
/// A channel box stays fully lit this long after a message, then fades.
const HOLD_S: f64 = 0.020;
const FADE_S: f64 = 0.250;
/// Short notes (drum triggers) still get a box this tall.
const MIN_BOX_PX: f32 = 7.0;

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    /// Source channel 0..16; `None` means all.
    channel: Option<u8>,
    /// Seconds of history shown down the graph.
    span_s: f32,
    /// Octaves added to every note before drawing, to move it into register.
    octave: i32,
    style: NoteStyle,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            channel: None,
            span_s: 8.0,
            octave: 0,
            style: NoteStyle::default(),
        }
    }
}

impl Params {
    fn sanitize(&mut self) {
        self.channel = self.channel.filter(|&c| c < 16);
        self.span_s = self.span_s.clamp(0.5, 60.0);
        self.octave = self.octave.clamp(-4, 4);
        self.style.sanitize();
    }
}

struct Note {
    ch: u8,
    key: u8,
    vel: u8,
    on: f64,
    /// `None` while held.
    off: Option<f64>,
}

#[derive(Default)]
pub struct MidiMonitor {
    p: Params,
    /// Hub state as of the last update, for the "no device" text.
    connected: bool,
    missing: bool,
    notes: VecDeque<Note>,
    /// Time of the last message per channel.
    last: [Option<f64>; 16],
    count: [u64; 16],
    /// Last message on the source channel(s), for the readout.
    last_msg: Option<(u8, [u8; 3])>,
}

impl MidiMonitor {
    fn now(&self) -> f64 {
        crate::midi::now()
    }

    fn passes(&self, ch: u8) -> bool {
        self.p.channel.is_none_or(|c| c == ch)
    }

    /// Where a note is drawn after the octave shift.
    fn shown_key(&self, n: &Note) -> i32 {
        n.key as i32 + 12 * self.p.octave
    }

    fn held(&self) -> impl Iterator<Item = &Note> {
        self.notes
            .iter()
            .filter(|n| n.off.is_none() && self.passes(n.ch))
    }

    /// The connection went away: nothing is held any more.
    fn end_held(&mut self) {
        let now = self.now();
        for n in self.notes.iter_mut().filter(|n| n.off.is_none()) {
            n.off = Some(now);
        }
    }

    fn close_note(&mut self, ch: u8, key: Option<u8>, t: f64) {
        for n in self.notes.iter_mut().rev() {
            if n.off.is_none() && n.ch == ch && key.is_none_or(|k| k == n.key) {
                n.off = Some(t);
                if key.is_some() {
                    break;
                }
            }
        }
    }

    fn handle(&mut self, t: f64, b: [u8; 3]) {
        let ch = b[0] & 0x0F;
        self.last[ch as usize] = Some(t);
        self.count[ch as usize] += 1;
        if self.passes(ch) {
            self.last_msg = Some((ch, b));
        }
        match (b[0] & 0xF0, b[2]) {
            (0x90, v) if v > 0 => {
                // Retriggered without a note-off: end the old one here.
                self.close_note(ch, Some(b[1]), t);
                self.notes.push_back(Note {
                    ch,
                    key: b[1],
                    vel: v,
                    on: t,
                    off: None,
                });
                if self.notes.len() > RING {
                    self.notes.pop_front();
                }
            }
            (0x80, _) | (0x90, _) => self.close_note(ch, Some(b[1]), t),
            // All sound off / all notes off.
            (0xB0, _) if b[1] == 120 || b[1] == 123 => self.close_note(ch, None, t),
            _ => {}
        }
    }

    fn channel_boxes(&mut self, ui: &mut Ui) {
        let now = self.now();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            let size = vec2(20.0, 18.0);
            for ch in 0..16u8 {
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                let lit = match self.last[ch as usize] {
                    Some(t) if now - t <= HOLD_S => 1.0,
                    Some(t) => (-(now - t - HOLD_S) / FADE_S).exp() as f32,
                    None => 0.0,
                };
                let selected = self.p.channel == Some(ch);
                // Green when the channel passes the filter, amber when it's
                // active but filtered out by a single-channel source.
                let passes = self.p.channel.is_none() || selected;
                let flash = if passes { theme::VU_OK } else { theme::VU_WARN };
                let dim = Color32::from_gray(34);
                let fill = theme::lerp(dim, flash, lit);
                let p = ui.painter();
                p.rect_filled(rect, 1.0, fill);
                let stroke = if selected {
                    Stroke::new(1.5, theme::ON_ACCENT)
                } else {
                    Stroke::new(1.0, Color32::from_gray(52))
                };
                p.rect_stroke(rect, 1.0, stroke, egui::StrokeKind::Inside);
                let text = if lit > 0.5 {
                    Color32::BLACK
                } else {
                    Color32::from_gray(150)
                };
                p.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    format!("{}", ch + 1),
                    FontId::proportional(8.0),
                    text,
                );
                if resp.clicked() {
                    // Clicking the selected channel goes back to all.
                    self.p.channel = if selected { None } else { Some(ch) };
                }
                resp.on_hover_text(format!(
                    "channel {}: {} messages",
                    ch + 1,
                    self.count[ch as usize]
                ));
            }
        });
    }
}

fn describe(ch: u8, b: [u8; 3]) -> String {
    let c = ch + 1;
    match b[0] & 0xF0 {
        0x90 if b[2] > 0 => format!("ch{c} on  {} v{}", pitch::note_name(b[1] as i32), b[2]),
        0x80 | 0x90 => format!("ch{c} off {}", pitch::note_name(b[1] as i32)),
        0xA0 => format!("ch{c} poly AT {} {}", pitch::note_name(b[1] as i32), b[2]),
        0xB0 => format!("ch{c} CC{} = {}", b[1], b[2]),
        0xC0 => format!("ch{c} program {}", b[1]),
        0xD0 => format!("ch{c} AT {}", b[1]),
        _ => format!("ch{c} bend {}", ((b[2] as i32) << 7 | b[1] as i32) - 8192),
    }
}

impl Module for MidiMonitor {
    fn title(&self) -> &'static str {
        "MIDI"
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

    fn graph_height(&self) -> f32 {
        220.0
    }

    fn lookback(&self, _rate: u32) -> usize {
        0
    }

    fn uses_audio(&self) -> bool {
        false
    }

    fn needs_midi(&self) -> bool {
        true
    }

    fn always_live(&self) -> bool {
        // Note on/off arrive once; skipping them would leave notes stuck held.
        true
    }

    fn update(&mut self, shared: &mut Shared) {
        let midi = &shared.midi;
        (self.connected, self.missing) = (midi.connected, midi.missing);
        for e in midi.events.iter() {
            self.handle(e.t, e.b);
        }
        if midi.dropped {
            self.end_held();
        }
    }

    fn marks(&self, out: &mut Vec<f32>) {
        out.extend(self.held().map(|n| self.shown_key(n) as f32));
    }

    fn has_top_row(&self) -> bool {
        true
    }

    fn top_row(&mut self, ui: &mut Ui) {
        // No open port means no channels to pick from; the port itself is
        // chosen in the app's top bar. The saved filter is kept meanwhile.
        if !self.connected {
            return;
        }
        ui.label("Source");
        if ui
            .selectable_label(self.p.channel.is_none(), "all")
            .clicked()
        {
            self.p.channel = None;
        }
        self.channel_boxes(ui);
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        ui.add(
            egui::Slider::new(&mut self.p.span_s, 0.5..=60.0)
                .logarithmic(true)
                .suffix(" s")
                .text("history"),
        );
        ui.horizontal(|ui| {
            ui.label("octave");
            if ui.small_button("−").clicked() {
                self.p.octave -= 1;
            }
            let o = self.p.octave;
            let text = if o == 0 { "0".into() } else { format!("{o:+}") };
            if ui
                .add(egui::Label::new(egui::RichText::new(text).monospace()).sense(Sense::click()))
                .on_hover_text("Shift notes up/down by octaves; click to reset")
                .clicked()
            {
                self.p.octave = 0;
            }
            if ui.small_button("+").clicked() {
                self.p.octave += 1;
            }
        });
        self.p.style.ui(ui);
        ui.horizontal(|ui| {
            if ui
                .button("fit range")
                .on_hover_text("Set the shared note range to the notes in the history")
                .clicked()
            {
                let keys = self
                    .notes
                    .iter()
                    .filter(|n| self.passes(n.ch))
                    .map(|n| self.shown_key(n));
                if let (Some(a), Some(b)) = (keys.clone().min(), keys.max()) {
                    let pad = ((NoteRange::MIN_KEYS - (b - a)).max(0) + 1) / 2 + 2;
                    shared.range.set(
                        NoteRange {
                            lo: a - pad,
                            hi: b + pad,
                            ..shared.range.get()
                        }
                        .sanitized(),
                    );
                }
            }
            if ui.button("clear").clicked() {
                self.notes.clear();
                self.count = [0; 16];
                self.last_msg = None;
            }
        });
        self.p.sanitize();
        let msg = self.last_msg.map_or("—".into(), |(ch, b)| describe(ch, b));
        ui.label(egui::RichText::new(msg).monospace().weak());
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, true);
    }

    fn layer_depth(&self) -> Option<u8> {
        Some(3)
    }

    fn layer(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, false);
    }
}

impl MidiMonitor {
    /// `full`: with key lanes, time grid and status text; otherwise the
    /// notes and held keys alone.
    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared, full: bool) {
        let now = self.now();
        let range = shared.range.get();
        let (lo, hi) = (range.lo, range.hi);
        let span = self.p.span_s as f64;
        let plot = if full {
            Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() - 14.0))
        } else {
            rect.with_max_y(rect.bottom() - 3.0)
        };
        let kw = plot.width() / range.count() as f32;
        // Left edge of key `k`'s lane: lanes are centred on the note.
        let x_scale = range.scale(plot);
        let key_x = |k: i32| x_scale.px(k as f32 - 0.5);
        let time_y = |t: f64| plot.top() + ((now - t) / span) as f32 * plot.height();
        let p = ui.painter_at(rect);

        if full {
            // Lanes: black keys slightly lighter, a line at each C.
            for k in lo..=hi {
                let x = key_x(k);
                if is_black(k) {
                    p.rect_filled(
                        Rect::from_min_max(pos2(x, plot.top()), pos2(x + kw, plot.bottom())),
                        0.0,
                        Color32::from_gray(19),
                    );
                }
                if k.rem_euclid(12) == 0 {
                    p.line_segment(
                        [pos2(x, plot.top()), pos2(x, rect.bottom())],
                        Stroke::new(1.0, Color32::from_gray(40)),
                    );
                    p.text(
                        pos2(x + 2.0, rect.bottom() - 1.0),
                        Align2::LEFT_BOTTOM,
                        pitch::note_name(k),
                        FontId::proportional(9.0),
                        Color32::from_gray(120),
                    );
                }
            }
            // Seconds ago.
            let step = [0.25, 0.5, 1.0, 2.0, 5.0, 10.0]
                .into_iter()
                .find(|s| span / s <= 10.0)
                .unwrap_or(10.0);
            let mut s = step;
            while s < span {
                let y = time_y(now - s);
                p.line_segment(
                    [pos2(plot.left(), y), pos2(plot.right(), y)],
                    Stroke::new(1.0, Color32::from_gray(30)),
                );
                p.text(
                    pos2(plot.right() - 3.0, y - 1.0),
                    Align2::RIGHT_BOTTOM,
                    format!("-{s}s"),
                    FontId::proportional(9.0),
                    Color32::from_gray(90),
                );
                s += step;
            }
        }
        let (mut below, mut above) = (0, 0);
        let style = &self.p.style;
        let min_hold = MIN_BOX_PX as f64 * span / plot.height().max(1.0) as f64;
        let (mut ghosts, mut mesh, mut edges) = (Mesh::default(), Mesh::default(), Vec::new());
        let (dim, bright) = (
            style.tint(theme::ACCENT).gamma_multiply(0.6),
            style.tint(theme::ACCENT_BRIGHT),
        );
        for n in &self.notes {
            if !self.passes(n.ch) || now - n.off.map_or(now, |t| t + style.tail_s()) > span {
                continue;
            }
            let k = self.shown_key(n);
            let base = theme::lerp(dim, bright, style.vel_gain(n.vel));
            let fill = if n.off.is_none() {
                theme::lerp(base, theme::ON_ACCENT, 0.35)
            } else {
                base
            };
            let draw = |m: &mut Mesh, k: i32, gain: f32, ghost: bool| {
                let x = style.span(key_x(k), kw, ghost);
                style.add_note(m, x, fill, gain, (n.on, n.off), now, min_hold, &time_y);
                x
            };
            for (&(dk, _), &g) in OVERTONES.iter().zip(&style.overtones) {
                if g > 0.0 && (lo..=hi).contains(&(k + dk)) {
                    draw(&mut ghosts, k + dk, g * 0.6, true);
                }
            }
            if k < lo || k > hi {
                if k < lo {
                    below += 1
                } else {
                    above += 1
                }
                continue;
            }
            let x = draw(&mut mesh, k, 1.0, false);
            edges.push((x, time_y(n.on)));
            if let Some(off) = n.off {
                edges.push((x, time_y(off)));
            }
        }
        p.add(Shape::mesh(ghosts));
        p.add(Shape::mesh(mesh));
        // Exact note-on / note-off edges.
        if style.edges > 0.0 {
            let edge = Stroke::new(1.5, theme::ON_ACCENT.gamma_multiply(style.edges));
            for (x, y) in edges {
                p.hline(x, y, edge);
            }
        }
        // Notes the key range hides.
        let small = FontId::proportional(10.0);
        if full && below > 0 {
            let text = p.text(
                plot.left_top() + vec2(12.0, 3.0),
                Align2::LEFT_TOP,
                format!("{below} below"),
                small.clone(),
                theme::ACCENT_BRIGHT,
            );
            icons::triangle(
                &p,
                pos2(plot.left() + 7.0, text.center().y),
                Dir::Left,
                3.5,
                theme::ACCENT_BRIGHT,
            );
        }
        if full && above > 0 {
            let text = p.text(
                plot.right_top() + vec2(-12.0, 3.0),
                Align2::RIGHT_TOP,
                format!("{above} above"),
                small,
                theme::ACCENT_BRIGHT,
            );
            icons::triangle(
                &p,
                pos2(plot.right() - 7.0, text.center().y),
                Dir::Right,
                3.5,
                theme::ACCENT_BRIGHT,
            );
        }

        // Held notes light up their key along the bottom.
        for n in self.held() {
            let k = self.shown_key(n);
            if (lo..=hi).contains(&k) {
                let x = key_x(k);
                p.rect_filled(
                    Rect::from_min_max(pos2(x, plot.bottom()), pos2(x + kw, plot.bottom() + 3.0)),
                    0.0,
                    theme::ACCENT_BRIGHT,
                );
            }
        }

        if full && !self.connected {
            let text = if self.missing {
                "no device"
            } else {
                "no MIDI input selected"
            };
            p.text(
                plot.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(13.0),
                Color32::from_gray(90),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_notes() {
        let mut m = MidiMonitor::default();
        m.handle(0.0, [0x92, 40, 100]); // ch3 on
        m.handle(0.1, [0x92, 40, 90]); // retrigger closes the first
        m.handle(0.2, [0x82, 40, 0]);
        m.handle(0.3, [0x90, 50, 64]);
        m.handle(0.4, [0x90, 50, 0]); // velocity-0 note-on is an off
        m.handle(0.5, [0x91, 60, 64]);
        m.handle(0.6, [0xB1, 123, 0]); // all notes off
        let offs: Vec<_> = m.notes.iter().map(|n| n.off).collect();
        assert_eq!(offs, [Some(0.1), Some(0.2), Some(0.4), Some(0.6)]);
        assert_eq!(m.count[2], 3);
        assert!(m.last[1].is_some() && m.last[5].is_none());
    }
}

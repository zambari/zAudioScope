//! The shared note / frequency range: one span of notes that every
//! frequency-axis analyser (tuner, spectrogram, waterfall, MIDI roll) uses for its x
//! axis, so a given pitch sits at the same x in all of them.
//!
//! The axis is linear in MIDI note number, i.e. logarithmic in frequency
//! (A4 = 440 Hz). Note `m` is centred at `m`, its column spans `m ± 0.5`.
//!
//! [`bar`] draws the control: 20 Hz–2 kHz log labels over a piano-roll
//! strip, with the selected range lit. Drag an edge to move one end, drag
//! inside to move both, scroll to shift, double-click to reset.

use egui::{Align2, Color32, CursorIcon, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use super::plot::{self, Scale};
use super::theme;
use super::widgets::{Grab, clamp_pair};
use crate::pitch;

/// Reference tuning of the shared axis.
pub const A4: f32 = 440.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteRange {
    /// Lowest and highest note shown, MIDI, inclusive.
    pub lo: i32,
    pub hi: i32,
}

impl Default for NoteRange {
    fn default() -> Self {
        Self { lo: 24, hi: 59 } // C1 .. B3
    }
}

impl NoteRange {
    /// Selectable limits: E0 (20.6 Hz) .. B6 (1976 Hz).
    pub const BOUNDS: Self = Self { lo: 16, hi: 95 };
    /// At least an octave is shown.
    pub const MIN_KEYS: i32 = 12;

    pub fn sanitized(self) -> Self {
        let (b, gap) = (Self::BOUNDS, Self::MIN_KEYS - 1);
        let hi = self.hi.clamp(b.lo + gap, b.hi);
        Self { lo: self.lo.clamp(b.lo, hi - gap), hi }
    }

    /// Shown span in fractional MIDI: half a note beyond each end note.
    pub fn span(self) -> (f32, f32) {
        (self.lo as f32 - 0.5, self.hi as f32 + 0.5)
    }

    /// The shown span's edge frequencies, Hz.
    pub fn hz(self) -> (f32, f32) {
        let (a, b) = self.span();
        (pitch::midi_to_freq(a, A4), pitch::midi_to_freq(b, A4))
    }

    pub fn count(self) -> usize {
        (self.hi - self.lo + 1) as usize
    }

    pub fn notes(self) -> std::ops::RangeInclusive<i32> {
        self.lo..=self.hi
    }

    /// Fractional MIDI note → x across `rect`.
    pub fn scale(self, rect: Rect) -> Scale {
        Scale::linear(self.span(), (rect.left(), rect.right()))
    }

    /// Frequency → x across `rect` (the same mapping as [`Self::scale`]).
    pub fn hz_scale(self, rect: Rect) -> Scale {
        Scale::log(self.hz(), (rect.left(), rect.right()))
    }

    /// Where `m` sits across the full selectable span, 0..1.
    pub fn bounds_pos(m: f32) -> f32 {
        let (a, b) = Self::BOUNDS.span();
        (m - a) / (b - a)
    }
}

pub fn is_black(key: i32) -> bool {
    matches!(key.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

/// Pick-up distance of an edge handle, px.
const EDGE_PX: f32 = 6.0;

/// The range control. `marks` are live pitches (fractional MIDI) reported
/// by the analysers, lit on the keys. Returns true when the range changed.
pub fn bar(ui: &mut Ui, range: &mut NoteRange, marks: &[f32]) -> bool {
    let before = *range;
    ui.horizontal(|ui| {
        // Readout in the parameter column, so the strip lines up with graphs.
        ui.allocate_ui_with_layout(vec2(super::PARAMS_W, 34.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(super::PARAMS_W);
            let (f0, f1) = range.hz();
            ui.label(egui::RichText::new(format!("{} – {}", pitch::note_name(range.lo), pitch::note_name(range.hi))).strong());
            ui.weak(format!("{} – {} Hz · {} notes", plot::fmt_hz(f0.round()), plot::fmt_hz(f1.round()), range.count()));
        });
        strip(ui, range, marks);
    });
    *range != before
}

fn strip(ui: &mut Ui, range: &mut NoteRange, marks: &[f32]) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click_and_drag());
    let full = NoteRange::BOUNDS;
    let notes = full.scale(rect);
    let label_h = 12.0;
    let keys = Rect::from_min_max(pos2(rect.left(), rect.top() + label_h), rect.max);

    // ---- interaction ------------------------------------------------------
    let (mut lo, mut hi) = (range.lo as f32, range.hi as f32);
    let edge_x = |v: f32, hi_end: bool| notes.px(v + if hi_end { 0.5 } else { -0.5 });
    let gap = (NoteRange::MIN_KEYS - 1) as f32;
    let bounds = (full.lo as f32, full.hi as f32);
    let id = resp.id.with("grab");
    let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
    let pick = |x: f32, lo: f32, hi: f32| {
        let (dl, dh) = ((x - edge_x(lo, false)).abs(), (x - edge_x(hi, true)).abs());
        if dl.min(dh) > EDGE_PX && x > edge_x(lo, false) && x < edge_x(hi, true) {
            Grab::Both
        } else if dl < dh {
            Grab::Lo
        } else {
            Grab::Hi
        }
    };
    if resp.double_clicked() {
        *range = NoteRange::default();
        return;
    }
    // The grab held by an ongoing drag, else the one the pointer hovers.
    let mut active: Option<Grab> = None;
    if let Some(p) = pointer {
        if resp.drag_started() || resp.clicked() {
            // Pick from where the button went down: drag_started fires only
            // after the pointer passed the drag threshold, by which time an
            // edge grab could already read as "inside" (→ Both, stuck at bounds).
            let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(p);
            let g = pick(origin.x, lo, hi);
            // Remember where the drag began, so moving both keeps whole notes.
            ui.data_mut(|d| d.insert_temp(id, (g, notes.value(origin.x), lo, hi)));
        }
        if resp.dragged() || resp.clicked() {
            if let Some((g, m0, lo0, hi0)) = ui.data(|d| d.get_temp::<(Grab, f32, f32, f32)>(id)) {
                let m = notes.value(p.x);
                match g {
                    Grab::Lo => lo = m.round(),
                    Grab::Hi => hi = m.round(),
                    Grab::Both => {
                        let d = (m - m0).round();
                        (lo, hi) = (lo0 + d, hi0 + d);
                    }
                }
                clamp_pair(&mut lo, &mut hi, bounds, gap, g);
            }
        }
        if resp.hovered() {
            // Scroll shifts the window a note per notch-ish.
            let s = ui.input(|i| i.smooth_scroll_delta);
            let d = if s.x.abs() > s.y.abs() { s.x } else { s.y };
            let acc_id = resp.id.with("scroll");
            let acc = ui.data(|m| m.get_temp::<f32>(acc_id).unwrap_or(0.0)) + d / 24.0;
            let whole = acc.trunc();
            ui.data_mut(|m| m.insert_temp(acc_id, acc - whole));
            if whole != 0.0 {
                (lo, hi) = (lo - whole, hi - whole);
                clamp_pair(&mut lo, &mut hi, bounds, gap, Grab::Both);
            }
        }
        active = if resp.dragged() { ui.data(|d| d.get_temp::<(Grab, f32, f32, f32)>(id)).map(|t| t.0) } else if resp.hovered() { Some(pick(p.x, lo, hi)) } else { None };
        if active.is_some() {
            ui.ctx().set_cursor_icon(if active == Some(Grab::Both) { CursorIcon::Grab } else { CursorIcon::ResizeHorizontal });
        }
    }
    *range = NoteRange { lo: lo as i32, hi: hi as i32 }.sanitized();

    if !ui.is_rect_visible(rect) {
        return;
    }
    // ---- drawing ----------------------------------------------------------
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);

    // Hz labels: log ticks across the full span.
    let hz = full.hz_scale(rect);
    for t in hz.ticks(34.0) {
        let x = hz.px(t.value);
        p.vline(x, rect.top()..=rect.top() + 3.0, Stroke::new(1.0, plot::TICK));
        let c = if t.major { Color32::from_gray(140) } else { Color32::from_gray(95) };
        p.text(pos2(x, rect.top() + 1.0), Align2::CENTER_TOP, plot::fmt_hz(t.value), FontId::monospace(9.0), c);
    }

    // Piano-roll keys: equal-width columns (log frequency), white / black.
    let kw = keys.width() / full.count() as f32;
    for k in full.notes() {
        let x0 = notes.px(k as f32 - 0.5);
        let black = is_black(k);
        let fill = if black { Color32::from_gray(26) } else { Color32::from_gray(78) };
        p.rect_filled(Rect::from_min_max(pos2(x0, keys.top()), pos2(x0 + kw, keys.bottom())), 0.0, fill);
        if k.rem_euclid(12) == 0 {
            p.vline(x0, keys.y_range(), Stroke::new(1.0, Color32::from_gray(30)));
            p.text(pos2(x0 + 1.5, keys.bottom() - 1.0), Align2::LEFT_BOTTOM, pitch::note_name(k), FontId::proportional(8.0), Color32::from_gray(30));
        } else if !black && !is_black(k - 1) {
            // E|F and B|C: white keys that touch.
            p.vline(x0, keys.y_range(), Stroke::new(1.0, Color32::from_gray(52)));
        }
    }

    // Dim outside the selection; outline and edge handles on it.
    let sel = Rect::from_min_max(pos2(edge_x(range.lo as f32, false), keys.top()), pos2(edge_x(range.hi as f32, true), keys.bottom()));
    let shade = Color32::from_black_alpha(175);
    p.rect_filled(Rect::from_min_max(keys.min, pos2(sel.left(), keys.bottom())), 0.0, shade);
    p.rect_filled(Rect::from_min_max(pos2(sel.right(), keys.top()), keys.max), 0.0, shade);
    p.rect_filled(sel, 0.0, theme::ACCENT.gamma_multiply(0.18));
    p.rect_stroke(sel, 1.0, Stroke::new(1.0, theme::ACCENT_BRIGHT), egui::StrokeKind::Inside);
    for (x, g) in [(sel.left() + 1.0, Grab::Lo), (sel.right() - 1.0, Grab::Hi)] {
        let lit = active == Some(g) || active == Some(Grab::Both);
        p.vline(x, keys.y_range(), Stroke::new(3.0, if lit { Color32::WHITE } else { theme::HANDLE }));
    }

    // Live pitches from the analysers.
    for &m in marks {
        let k = m.round() as i32;
        if !full.notes().contains(&k) {
            continue;
        }
        let x0 = notes.px(k as f32 - 0.5);
        p.rect_filled(Rect::from_min_max(pos2(x0, keys.bottom() - 5.0), pos2(x0 + kw, keys.bottom())), 0.0, theme::ACCENT_BRIGHT);
        p.vline(notes.px(m), keys.y_range(), Stroke::new(1.0, theme::ON_ACCENT));
    }

    if let Some(h) = resp.hover_pos() {
        let m = notes.value(h.x);
        resp.on_hover_text_at_pointer(format!(
            "{} · {} Hz\nDrag an edge or the selection, scroll to shift, double-click to reset.",
            pitch::note_name(m.round() as i32),
            plot::trim(pitch::midi_to_freq(m, A4).round())
        ));
    }
}

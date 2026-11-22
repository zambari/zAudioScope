//! Small reusable parameter-column widgets.
//!
//! - [`Foldout`]: a collapsible sub-section inside a module's parameters.
//!   Its open/closed state is kept in egui's persisted memory, so it comes
//!   back the way it was left, across frames and app restarts.
//! - [`RangeSlider`]: one track with two handles for a `lo ..= hi` pair
//!   (floor / ceiling, from / to). Drag a handle to move one end, drag the
//!   filled span to move both, scroll over the track to shift the window.

use std::ops::RangeInclusive;

use egui::{
    Align2, AsIdSalt, CursorIcon, FontId, Frame, Margin, Response, Sense, Stroke, Ui, Widget, pos2,
    vec2,
};

/// Stand-in for a graph while there is no input to show (see
/// [`Shared::idle_label`](crate::modules::Shared::idle_label)), centred in `rect`.
pub fn placeholder(ui: &Ui, rect: egui::Rect, label: &str) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(14.0),
        ui.visuals().weak_text_color(),
    );
}

/// Max height of the device and channel popups (twice egui's default of 200).
pub const POPUP_HEIGHT: f32 = 400.0;

/// Device names longer than this are cut with an ellipsis in the list; the
/// full name shows as a hover tooltip.
const DEVICE_NAME_MAX: usize = 40;

/// `name` cut to `DEVICE_NAME_MAX` characters, with "…" when shortened.
pub fn short_device_name(name: &str) -> String {
    if name.chars().count() <= DEVICE_NAME_MAX {
        return name.to_owned();
    }
    let kept: String = name.chars().take(DEVICE_NAME_MAX - 1).collect();
    format!("{}…", kept.trim_end())
}

/// Audio device dropdown. Returns the id picked this frame.
pub fn device_picker(
    ui: &mut Ui,
    salt: &str,
    width: f32,
    selected: &str,
    current: Option<&str>,
    devices: &[crate::audio::DeviceInfo],
) -> (Response, Option<String>) {
    let mut picked = None;
    let r = egui::ComboBox::from_id_salt(salt)
        .width(width)
        .height(POPUP_HEIGHT)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for d in devices {
                if ui
                    .selectable_label(current == Some(d.id.as_str()), short_device_name(&d.name))
                    .on_hover_text(&d.name)
                    .clicked()
                {
                    picked = Some(d.id.clone());
                }
            }
        });
    (r.response, picked)
}

/// Module foldout with an independent, right-aligned display switch.
pub fn module_header(
    ui: &mut Ui,
    title: &str,
    open: &mut bool,
    enabled: &mut bool,
) -> (egui::Pos2, f32) {
    // Wide parameter rows (sliders, note toggles) can grow the scroll content
    // past the visible inspector; pin the header to the viewport's right edge
    // so the switch stays reachable however narrow the inspector gets.
    let width = ui
        .available_width()
        .min(ui.clip_rect().right() - ui.cursor().left())
        .max(0.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    let switch = egui::Rect::from_min_max(
        pos2((rect.right() - FLICK_HIT.x).max(rect.left()), rect.top()),
        rect.max,
    );
    ui.put(switch, flicktoggle(enabled))
        .on_hover_text(format!("Show {title} display"));
    let label = egui::Rect::from_min_max(
        rect.min,
        pos2(switch.left().max(rect.left()), rect.bottom()),
    );
    let font = FontId::proportional(13.0);
    let measure = |text: String| {
        ui.painter()
            .layout_no_wrap(text, font.clone(), ui.visuals().text_color())
            .size()
            .x
    };
    // Room for the disclosure triangle (drawn in the 16px before the title).
    let name_left = label.left() + 16.0;
    // The arrow plus the title: both fold the section.
    let name = egui::Rect::from_min_max(
        pos2(label.left(), label.top()),
        pos2(name_left + measure(title.to_owned()), label.bottom()),
    )
    .intersect(label);
    // Expanded sections only fold from the name; the remaining header still
    // selects the module for its connector without collapsing the controls.
    let hit = if *open { name } else { label };
    let response = ui.interact(hit, ui.scope_id().with("foldout"), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), title));
    if response.clicked() {
        *open = !*open;
    }
    let mut label_end = label.left_center();
    if ui.is_rect_visible(label) {
        if response.hovered() {
            ui.painter()
                .rect_filled(hit, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let text_color = ui.style().interact(&response).text_color();
        let clipped = ui.painter().with_clip_rect(label.intersect(ui.clip_rect()));
        super::icons::disclosure(&clipped, label.left_center() + vec2(8.0, 0.0), *open, text_color);
        let text_rect = clipped.text(
            label.left_center() + vec2(16.0, 0.0),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(13.0),
            text_color,
        );
        label_end = pos2(text_rect.right() + 35.0, text_rect.center().y);
    }
    (label_end, switch.left() - 4.0)
}

/// Drawn size of [`flicktoggle`].
const FLICK_SIZE: egui::Vec2 = vec2(44.0, 18.0);
/// Click area of [`flicktoggle`]: the drawn switch sits at its right edge,
/// with extra room to the left and above/below so near-misses still toggle
/// instead of folding the module header.
const FLICK_HIT: egui::Vec2 = vec2(FLICK_SIZE.x + 16.0, 22.0);

/// Compact flicktoggle: muted orange rail with a faint orange border when
/// on, dark rail with a grey outline when off.
pub fn flicktoggle(enabled: &mut bool) -> impl Widget + '_ {
    use super::theme;
    use egui::{Color32, StrokeKind};
    move |ui: &mut Ui| {
        let (hit, mut response) = ui.allocate_exact_size(FLICK_HIT, Sense::click());
        let rect = egui::Rect::from_min_size(
            pos2(
                hit.right() - FLICK_SIZE.x,
                hit.center().y - FLICK_SIZE.y / 2.0,
            ),
            FLICK_SIZE,
        );
        if response.clicked() {
            *enabled = !*enabled;
            response.mark_changed();
        }
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *enabled, "Display")
        });
        if ui.is_rect_visible(rect) {
            let lit = response.hovered() || response.has_focus();
            let (fill, border) = if *enabled {
                (
                    theme::lerp(Color32::from_gray(30), theme::ACCENT, 0.5),
                    theme::ACCENT_BRIGHT.gamma_multiply(if lit { 0.75 } else { 0.4 }),
                )
            } else {
                (
                    Color32::from_gray(28),
                    Color32::from_gray(if lit { 95 } else { 68 }),
                )
            };
            let p = ui.painter();
            p.rect_filled(rect, 4.0, fill);
            p.rect_stroke(rect, 4.0, Stroke::new(1.0, border), StrokeKind::Inside);
            let inner = rect.shrink(2.0);
            let width = 21.0;
            let x = if *enabled {
                inner.right() - width
            } else {
                inner.left()
            };
            let thumb =
                egui::Rect::from_min_size(pos2(x, inner.top()), vec2(width, inner.height()));
            let grey = if *enabled {
                170
            } else if lit {
                90
            } else {
                70
            };
            p.rect_filled(thumb, 3.0, Color32::from_gray(grey));
            let text = if *enabled {
                Color32::from_gray(30)
            } else {
                Color32::from_gray(190)
            };
            p.text(
                thumb.center(),
                Align2::CENTER_CENTER,
                if *enabled { "On" } else { "Off" },
                FontId::proportional(10.0),
                text,
            );
        }
        response
    }
}

// ---- Foldout ---------------------------------------------------------------

/// A collapsible sub-section of a parameter column.
///
/// ```ignore
/// Foldout::new("snap", "snap").summary("any semitone").show(ui, |ui| { ... });
/// ```
pub struct Foldout<S> {
    salt: S,
    title: String,
    summary: Option<String>,
    default_open: bool,
}

impl<S: AsIdSalt> Foldout<S> {
    /// `id_salt` keeps the remembered state; keep it stable (the title may
    /// change, the salt shouldn't).
    pub fn new(salt: S, title: impl Into<String>) -> Self {
        Self {
            salt,
            title: title.into(),
            summary: None,
            default_open: false,
        }
    }

    /// Short faint text at the right of the header, shown open or closed.
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = Some(s.into());
        self
    }

    #[allow(dead_code)]
    pub fn default_open(mut self, open: bool) -> Self {
        self.default_open = open;
        self
    }

    /// Draw the header and, when open, the body (slightly indented).
    pub fn show<R>(self, ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> Option<R> {
        let id = ui.make_persistent_id(&self.salt);
        let mut open = ui.data_mut(|d| *d.get_persisted_mut_or(id, self.default_open));

        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::click());
        if resp.clicked() {
            open = !open;
            ui.data_mut(|d| d.insert_persisted(id, open));
        }
        if ui.is_rect_visible(rect) {
            let v = ui.style().interact(&resp);
            let painter = ui.painter();
            if resp.hovered() {
                painter.rect_filled(
                    rect,
                    2.0,
                    ui.visuals()
                        .widgets
                        .hovered
                        .weak_bg_fill
                        .gamma_multiply(0.5),
                );
            }
            super::icons::disclosure(painter, rect.left_center() + vec2(6.0, 0.0), open, v.text_color());
            painter.text(
                rect.left_center() + vec2(16.0, 0.0),
                Align2::LEFT_CENTER,
                self.title,
                FontId::proportional(12.0),
                v.text_color(),
            );
            if let Some(s) = &self.summary {
                painter.text(
                    rect.right_center() - vec2(3.0, 0.0),
                    Align2::RIGHT_CENTER,
                    s,
                    FontId::proportional(10.0),
                    ui.visuals().weak_text_color(),
                );
            }
        }
        if !open {
            return None;
        }
        let inner = Frame::NONE
            .inner_margin(Margin {
                left: 10,
                right: 0,
                top: 0,
                bottom: 4,
            })
            .show(ui, body)
            .inner;
        Some(inner)
    }
}

// ---- RangeSlider -----------------------------------------------------------

/// Two handles on one track for a `lo ..= hi` pair, plus numeric fields.
pub struct RangeSlider<'a> {
    lo: &'a mut f32,
    hi: &'a mut f32,
    bounds: RangeInclusive<f32>,
    min_gap: f32,
    text: String,
    decimals: usize,
}

/// Which end of a range a drag moves.
#[derive(Clone, Copy, PartialEq)]
pub enum Grab {
    Lo,
    Hi,
    Both,
}

impl<'a> RangeSlider<'a> {
    pub fn new(lo: &'a mut f32, hi: &'a mut f32, bounds: RangeInclusive<f32>) -> Self {
        Self {
            lo,
            hi,
            bounds,
            min_gap: 0.0,
            text: String::new(),
            decimals: 0,
        }
    }

    /// Smallest allowed `hi - lo`.
    pub fn min_gap(mut self, gap: f32) -> Self {
        self.min_gap = gap;
        self
    }

    /// Label after the numeric fields.
    pub fn text(mut self, t: impl Into<String>) -> Self {
        self.text = t.into();
        self
    }

    #[allow(dead_code)]
    pub fn decimals(mut self, d: usize) -> Self {
        self.decimals = d;
        self
    }
}

/// Keep `lo < hi` with `gap` between them and both inside `[min, max]`,
/// moving the end that wasn't touched.
pub fn clamp_pair(lo: &mut f32, hi: &mut f32, (min, max): (f32, f32), gap: f32, moved: Grab) {
    let gap = gap.min(max - min);
    match moved {
        Grab::Lo => {
            *lo = lo.clamp(min, max - gap);
            *hi = hi.clamp(*lo + gap, max);
        }
        Grab::Hi => {
            *hi = hi.clamp(min + gap, max);
            *lo = lo.clamp(min, *hi - gap);
        }
        Grab::Both => {
            let w = (*hi - *lo).max(gap);
            *lo = lo.clamp(min, max - w);
            *hi = *lo + w;
        }
    }
}

impl Widget for RangeSlider<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let Self {
            lo,
            hi,
            bounds,
            min_gap,
            text,
            decimals,
        } = self;
        let (min, max) = (*bounds.start(), *bounds.end());
        let span = (max - min).max(f32::EPSILON);
        let (lo0, hi0) = (*lo, *hi);
        clamp_pair(lo, hi, (min, max), min_gap, Grab::Both);

        ui.horizontal(|ui| {
            let h = ui.spacing().interact_size.y;
            let w = ui.spacing().slider_width;
            let (rect, mut resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
            let r = 6.0;
            let rail = rect.shrink2(vec2(r, 0.0));
            let x_of = |v: f32| rail.left() + (v - min) / span * rail.width();
            let v_of = |x: f32| min + (x - rail.left()) / rail.width() * span;

            // Which part the drag holds, kept across frames of one drag.
            let grab_id = resp.id.with("grab");
            // Pick from the press origin: drag_started fires only past the
            // drag threshold, when the pointer may have left the handle.
            if let Some(p) = resp.interact_pointer_pos()
                && (resp.drag_started() || resp.clicked())
            {
                let p = ui.input(|i| i.pointer.press_origin()).unwrap_or(p);
                let (dl, dh) = ((p.x - x_of(*lo)).abs(), (p.x - x_of(*hi)).abs());
                let between = p.x > x_of(*lo) + r && p.x < x_of(*hi) - r;
                let g = if between && resp.drag_started() {
                    Grab::Both
                } else if dl < dh || (dl == dh && p.x < x_of(*lo)) {
                    Grab::Lo
                } else {
                    Grab::Hi
                };
                ui.data_mut(|d| d.insert_temp(grab_id, g));
                if g != Grab::Both {
                    // Jump the picked handle to the pointer.
                    let v = v_of(p.x);
                    if g == Grab::Lo {
                        *lo = v
                    } else {
                        *hi = v
                    }
                    clamp_pair(lo, hi, (min, max), min_gap, g);
                }
            } else if resp.dragged() {
                let g = ui
                    .data(|d| d.get_temp::<Grab>(grab_id))
                    .unwrap_or(Grab::Both);
                match g {
                    Grab::Both => {
                        let dv = resp.drag_delta().x / rail.width() * span;
                        *lo += dv;
                        *hi += dv;
                    }
                    Grab::Lo => *lo = v_of(resp.interact_pointer_pos().map_or(x_of(*lo), |p| p.x)),
                    Grab::Hi => *hi = v_of(resp.interact_pointer_pos().map_or(x_of(*hi), |p| p.x)),
                }
                clamp_pair(lo, hi, (min, max), min_gap, g);
            }
            // Scroll over the track shifts the whole window.
            if resp.hovered() {
                let s = ui.input(|i| i.smooth_scroll_delta);
                let d = if s.x.abs() > s.y.abs() { s.x } else { s.y };
                if d != 0.0 {
                    let dv = d / rail.width() * span * 0.5;
                    *lo += dv;
                    *hi += dv;
                    clamp_pair(lo, hi, (min, max), min_gap, Grab::Both);
                }
                let between = resp
                    .hover_pos()
                    .is_some_and(|p| p.x > x_of(*lo) + r && p.x < x_of(*hi) - r);
                ui.ctx().set_cursor_icon(if resp.dragged() || between {
                    CursorIcon::Grab
                } else {
                    CursorIcon::ResizeHorizontal
                });
            }

            if ui.is_rect_visible(rect) {
                let painter = ui.painter();
                let vis = ui.visuals();
                let wv = ui.style().interact(&resp);
                let cy = rect.center().y;
                let rail_rect = egui::Rect::from_min_max(
                    pos2(rail.left(), cy - 2.0),
                    pos2(rail.right(), cy + 2.0),
                );
                painter.rect_filled(rail_rect, 2.0, vis.widgets.inactive.bg_fill);
                let fill =
                    egui::Rect::from_min_max(pos2(x_of(*lo), cy - 2.0), pos2(x_of(*hi), cy + 2.0));
                painter.rect_filled(fill, 2.0, vis.selection.bg_fill);
                for v in [*lo, *hi] {
                    let c = pos2(x_of(v), cy);
                    painter.circle(
                        c,
                        r * 0.8 + wv.expansion * 0.5,
                        wv.bg_fill,
                        Stroke::new(1.0, wv.fg_stroke.color),
                    );
                }
            }

            let speed = span as f64 / 300.0;
            let (l, hh) = (*lo, *hi);
            let gap = min_gap.min(span);
            let dl = ui.add(
                egui::DragValue::new(lo)
                    .range(min..=hh - gap)
                    .speed(speed)
                    .max_decimals(decimals)
                    .min_decimals(decimals),
            );
            if dl.changed() {
                clamp_pair(lo, hi, (min, max), min_gap, Grab::Lo);
            }
            let dh = ui.add(
                egui::DragValue::new(hi)
                    .range(l + gap..=max)
                    .speed(speed)
                    .max_decimals(decimals)
                    .min_decimals(decimals),
            );
            if dh.changed() {
                clamp_pair(lo, hi, (min, max), min_gap, Grab::Hi);
            }
            if !text.is_empty() {
                ui.label(&text);
            }
            if *lo != lo0 || *hi != hi0 {
                resp.mark_changed();
            }
            resp.union(dl).union(dh)
        })
        .inner
    }
}

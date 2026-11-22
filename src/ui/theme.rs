//! Same look as KitPool: neutral dark grey base, dark orange accent,
//! nearly square corners.

use egui::{Color32, CornerRadius, Shadow, Stroke};

/// Fill for selected / toggled-on things.
pub const ACCENT: Color32 = Color32::from_rgb(184, 88, 18);
/// Brighter orange for text, outlines and markers on dark backgrounds.
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(236, 132, 44);
/// Text drawn on top of an `ACCENT` fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(255, 238, 222);
/// Main waveform colour.
pub const WAVE: Color32 = Color32::from_rgb(178, 200, 214);
/// Draggable graph handles (thresholds, zoom, speed).
pub const HANDLE: Color32 = Color32::from_rgb(222, 78, 60);
/// In tune / within tolerance.
pub const GOOD: Color32 = Color32::from_rgb(80, 200, 120);

/// Level-meter palette (green / amber / red), lowest zone first.
pub const VU_OK: Color32 = Color32::from_rgb(96, 168, 104);
pub const VU_WARN: Color32 = Color32::from_rgb(240, 205, 65);
pub const VU_CLIP: Color32 = Color32::from_rgb(222, 60, 50);

/// Straight RGB blend from `a` (u = 0) to `b` (u = 1); opaque result.
pub fn lerp(a: Color32, b: Color32, u: f32) -> Color32 {
    let u = u.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * u) as u8;
    Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
}

/// Rotate `c`'s hue by `turns` of the colour wheel, then fade it towards
/// `bg` by `fade` per channel: channels weak in the rotated colour's hue
/// fade faster (by `tint`, 0 = plain `lerp`), so fading leans into the hue.
pub fn hue_fade(bg: Color32, c: Color32, turns: f32, fade: f32, tint: f32) -> Color32 {
    let c = if turns == 0.0 {
        c
    } else {
        let mut h = egui::ecolor::Hsva::from(c);
        h.h = (h.h + turns).rem_euclid(1.0);
        Color32::from(h)
    };
    let fade = fade.clamp(0.0, 1.0);
    let peak = c.r().max(c.g()).max(c.b()).max(1) as f32;
    let ch = |x: u8, y: u8| {
        let u = fade.powf(1.0 + tint * (1.0 - y as f32 / peak));
        (x as f32 + (y as f32 - x as f32) * u) as u8
    };
    Color32::from_rgb(ch(bg.r(), c.r()), ch(bg.g(), c.g()), ch(bg.b(), c.b()))
}

/// Orange bar along the left edge of the selected module, both its
/// inspector foldout and its display panel.
pub fn selected_edge(painter: &egui::Painter, x: f32, y: egui::Rangef) {
    painter.vline(x, y, Stroke::new(2.0, ACCENT_BRIGHT));
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        let r = CornerRadius::same(2);
        let v = &mut style.visuals;

        v.panel_fill = Color32::from_gray(24);
        v.window_fill = Color32::from_gray(30);
        v.extreme_bg_color = Color32::from_gray(13);
        v.faint_bg_color = Color32::from_gray(32);
        v.window_stroke = Stroke::new(1.0, Color32::from_gray(56));
        v.window_corner_radius = CornerRadius::same(3);
        v.menu_corner_radius = CornerRadius::same(3);
        v.popup_shadow = Shadow { offset: [0, 4], blur: 10, spread: 0, color: Color32::from_black_alpha(100) };

        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, ON_ACCENT);
        v.hyperlink_color = ACCENT_BRIGHT;
        v.text_cursor.stroke = Stroke::new(2.0, ACCENT_BRIGHT);
        v.slider_trailing_fill = true;

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = Color32::from_gray(24);
        w.noninteractive.weak_bg_fill = Color32::from_gray(24);
        w.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_gray(48));
        w.inactive.bg_fill = Color32::from_gray(46);
        w.inactive.weak_bg_fill = Color32::from_gray(38);
        w.inactive.bg_stroke = Stroke::NONE;
        w.hovered.bg_fill = Color32::from_gray(58);
        w.hovered.weak_bg_fill = Color32::from_gray(52);
        w.hovered.bg_stroke = Stroke::new(1.0, ACCENT_BRIGHT.gamma_multiply(0.55));
        w.active.bg_fill = Color32::from_gray(66);
        w.active.weak_bg_fill = Color32::from_gray(62);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT_BRIGHT);
        w.open.bg_fill = Color32::from_gray(46);
        w.open.weak_bg_fill = Color32::from_gray(46);
        w.open.bg_stroke = Stroke::new(1.0, Color32::from_gray(70));
        for wv in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            wv.corner_radius = r;
            wv.expansion = 0.0;
        }

        style.spacing.button_padding = egui::vec2(6.0, 2.0);
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    });
}

/// Corner radius of a tab's top corners (the bottom is flat).
const TAB_RADIUS: f32 = 4.0;

/// A tab header (same look as sample_manager's pane tabs). Inactive tabs are
/// a soft grey block; the active one has no fill and an orange frame on top
/// and sides, open at the bottom so it reads as joined to the content below.
/// A `dim` tab (e.g. a switched-off analyzer) has weak text.
pub fn tab(label: &str, active: bool, dim: bool) -> impl egui::Widget + '_ {
    move |ui: &mut egui::Ui| {
        let pad = egui::vec2(10.0, 3.0);
        let font = egui::TextStyle::Button.resolve(ui.style());
        let color = match (active, dim) {
            (true, _) => ACCENT_BRIGHT,
            (false, true) => ui.visuals().weak_text_color(),
            (false, false) => ui.visuals().text_color(),
        };
        let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
        let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::click());
        if !ui.is_rect_visible(rect) {
            return resp;
        }
        let p = ui.painter();
        let rad = TAB_RADIUS as u8;
        let corners = CornerRadius { nw: rad, ne: rad, sw: 0, se: 0 };
        let w = &ui.visuals().widgets;
        if active {
            if resp.hovered() {
                p.rect_filled(rect, corners, w.inactive.weak_bg_fill.gamma_multiply(0.5));
            }
            let stroke = Stroke::new(1.0, ACCENT_BRIGHT.gamma_multiply(0.8));
            p.add(egui::Shape::line(open_tab_outline(rect.shrink(0.5)), stroke));
        } else {
            let fill = if resp.hovered() { w.hovered.weak_bg_fill } else { w.inactive.weak_bg_fill };
            p.rect_filled(rect, corners, fill);
        }
        p.galley(rect.min + pad, galley, color);
        resp
    }
}

/// A row of [`tab`]s; `current` is updated when one is clicked. A hairline
/// runs along the bottom, broken under the active tab. Each entry is
/// (value, label, dim).
pub fn tab_strip<T: Copy + PartialEq>(ui: &mut egui::Ui, tabs: &[(T, &str, bool)], current: &mut T) {
    let full = ui.available_rect_before_wrap();
    let mut gap = None;
    let row = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.add_space(6.0);
        for &(value, label, dim) in tabs {
            let resp = ui.add(tab(label, *current == value, dim));
            if resp.clicked() {
                *current = value;
            }
            if *current == value {
                gap = Some(resp.rect.x_range());
            }
        }
    });
    let y = row.response.rect.bottom() - 0.5;
    let stroke = Stroke::new(1.0, ACCENT_BRIGHT.gamma_multiply(0.8));
    let p = ui.painter();
    match gap {
        Some(g) => {
            p.hline(full.left()..=g.min + 0.5, y, stroke);
            p.hline(g.max - 0.5..=full.right(), y, stroke);
        }
        None => {
            p.hline(full.x_range(), y, stroke);
        }
    }
}

/// Left side, rounded top, right side — no bottom edge.
fn open_tab_outline(r: egui::Rect) -> Vec<egui::Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI};
    let rad = TAB_RADIUS.min(r.width() / 2.0).min(r.height());
    let arc = |c: egui::Pos2, from: f32, to: f32, out: &mut Vec<egui::Pos2>| {
        const STEPS: usize = 6;
        for i in 0..=STEPS {
            let a = from + (to - from) * i as f32 / STEPS as f32;
            out.push(c + rad * egui::vec2(a.cos(), a.sin()));
        }
    };
    let mut pts = vec![r.left_bottom()];
    arc(egui::pos2(r.left() + rad, r.top() + rad), PI, PI + FRAC_PI_2, &mut pts);
    arc(egui::pos2(r.right() - rad, r.top() + rad), -FRAC_PI_2, 0.0, &mut pts);
    pts.push(r.right_bottom());
    pts
}

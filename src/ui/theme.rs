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

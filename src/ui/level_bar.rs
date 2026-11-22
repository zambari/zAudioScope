//! Unit-independent segmented level/progress bar. Callers supply normalized values.
use egui::{Color32, Painter, Rect, Stroke, pos2};

/// Paint a clamped 0..1 fill and optional peak marker into an existing rectangle.
/// Background, allocation, labels and any smoothing belong to the caller.
pub fn paint(painter: &Painter, rect: Rect, level: f32, peak: Option<f32>) {
    let level = level.clamp(0.0, 1.0);
    let x_of = |value: f32| rect.left() + rect.width() * value;
    for (start, end, color) in [
        (0.0_f32, 0.8_f32, Color32::from_rgb(96, 168, 104)),
        (0.8, 0.9, Color32::from_rgb(240, 205, 65)),
        (0.9, 1.0, Color32::from_rgb(222, 60, 50)),
    ] {
        let end = end.min(level);
        if end > start {
            painter.rect_filled(
                Rect::from_x_y_ranges(x_of(start)..=x_of(end), rect.y_range()),
                0.0,
                color,
            );
        }
    }
    if let Some(peak) = peak {
        let x = x_of(peak.clamp(0.0, 1.0));
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0, Color32::from_gray(220)),
        );
    }
}

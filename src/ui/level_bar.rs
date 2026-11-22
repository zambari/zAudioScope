//! Unit-independent segmented level/progress bar. Callers supply normalized values.
use super::theme::{VU_CLIP, VU_OK, VU_WARN};
use egui::{Color32, Painter, Rect, Stroke, pos2};

/// Paint a clamped 0..1 fill and optional peak marker into an existing rectangle.
/// Background, allocation, labels and any smoothing belong to the caller.
pub fn paint(painter: &Painter, rect: Rect, level: f32, peak: Option<f32>) {
    let level = level.clamp(0.0, 1.0);
    let x_of = |value: f32| rect.left() + rect.width() * value;
    for (start, end, color) in [
        (0.0_f32, 0.8_f32, VU_OK),
        (0.8, 0.9, VU_WARN),
        (0.9, 1.0, VU_CLIP),
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

/// Exponential-moving-average weight for a sample arriving `dt` seconds after the last,
/// given a smoothing time constant `tau` (seconds). Returns 1.0 for non-positive `dt`.
pub fn ema_alpha(dt: f32, tau: f32) -> f32 {
    if dt > 0.0 { 1.0 - (-dt / tau).exp() } else { 1.0 }
}

/// Moving-averaged bar value with a falling peak marker that tracks the raw input,
/// for noisy signals such as load/CPU diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smoothed {
    average: Option<f32>,
    peak: f32,
}

impl Smoothed {
    /// Smoothing time constant of the bar fill, seconds.
    const TAU: f32 = 0.5;
    /// Peak marker fall rate, full-scale per second.
    const PEAK_FALL: f32 = 0.25;

    pub fn update(&mut self, value: f32, dt: f32) {
        let average = self.average.get_or_insert(value);
        *average += ema_alpha(dt, Self::TAU) * (value - *average);
        self.peak = value.max(self.peak - Self::PEAK_FALL * dt.max(0.0));
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn paint(&self, painter: &Painter, rect: Rect) {
        paint(painter, rect, self.average.unwrap_or(0.0), Some(self.peak));
    }
}

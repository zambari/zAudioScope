//! Trigger scope. In Trigger mode it locks onto an impulse (a kick) and
//! shows the audio after it; the shaded window (drag on the graph, or use
//! the sliders) becomes the gate the other modules analyse. Free mode just
//! scrolls, and the others analyse everything.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui};

use super::{Gate, Module, Shared};
use crate::ui::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Free,
    Trigger,
}

pub struct Scope {
    mode: Mode,
    /// Trigger level, 0..1 of full scale.
    threshold: f32,
    /// After a trigger, ignore further crossings for this long.
    holdoff_ms: f32,
    /// Time shown after the trigger (or in total, in free mode).
    view_ms: f32,
    /// Gate: analyse from `win_start_ms` to `win_end_ms` after the trigger.
    win_start_ms: f32,
    win_end_ms: f32,
    gain: f32,
    /// Absolute sample index of the trigger being shown.
    trigger: Option<u64>,
    drag_from_ms: Option<f32>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            mode: Mode::Trigger,
            threshold: 0.3,
            holdoff_ms: 200.0,
            view_ms: 600.0,
            win_start_ms: 80.0,
            win_end_ms: 400.0,
            gain: 1.0,
            trigger: None,
            drag_from_ms: None,
        }
    }
}

impl Scope {
    /// Latest rising crossing of the threshold that has a full gate window
    /// of audio after it and is past the holdoff of the previous one.
    fn find_trigger(&self, shared: &Shared) -> Option<u64> {
        let f = &shared.frame;
        let x = &f.samples;
        let need = f.ms_to_samples(self.win_end_ms.max(self.view_ms));
        let holdoff = f.ms_to_samples(self.holdoff_ms);
        let last = x.len().checked_sub(need)?;
        let mut found = None;
        let mut i = 1;
        while i <= last {
            if x[i].abs() >= self.threshold && x[i - 1].abs() < self.threshold {
                found = Some(f.start + i as u64);
                i += holdoff.max(1);
            } else {
                i += 1;
            }
        }
        found
    }
}

impl Module for Scope {
    fn title(&self) -> &'static str {
        "Scope"
    }

    fn graph_height(&self) -> f32 {
        220.0
    }

    fn update(&mut self, shared: &mut Shared) {
        match self.mode {
            Mode::Free => shared.gate = Gate { window: None },
            Mode::Trigger => {
                if let Some(t) = self.find_trigger(shared) {
                    self.trigger = Some(t);
                }
                let f = &shared.frame;
                shared.gate.window = self.trigger.map(|t| {
                    (t + f.ms_to_samples(self.win_start_ms) as u64, t + f.ms_to_samples(self.win_end_ms) as u64)
                });
            }
        }
    }

    fn params(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, Mode::Trigger, "Trigger");
            ui.selectable_value(&mut self.mode, Mode::Free, "Free");
        });
        ui.add(egui::Slider::new(&mut self.view_ms, 50.0..=2000.0).text("view ms"));
        ui.add(egui::Slider::new(&mut self.gain, 0.5..=16.0).logarithmic(true).text("gain"));
        ui.add_enabled_ui(self.mode == Mode::Trigger, |ui| {
            ui.add(egui::Slider::new(&mut self.threshold, 0.01..=1.0).text("threshold"));
            ui.add(egui::Slider::new(&mut self.holdoff_ms, 0.0..=1000.0).text("holdoff ms"));
            ui.add(egui::Slider::new(&mut self.win_start_ms, 0.0..=self.view_ms).text("gate from"));
            ui.add(egui::Slider::new(&mut self.win_end_ms, 0.0..=self.view_ms).text("gate to"));
        });
        if self.win_end_ms < self.win_start_ms {
            std::mem::swap(&mut self.win_start_ms, &mut self.win_end_ms);
        }
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        let f = &shared.frame;
        let view = f.ms_to_samples(self.view_ms).max(2);
        let start = match (self.mode, self.trigger) {
            (Mode::Trigger, Some(t)) => t,
            _ => f.start + f.samples.len().saturating_sub(view) as u64,
        };
        let painter = ui.painter_at(rect);
        let x_of_ms = |ms: f32| rect.left() + rect.width() * ms / self.view_ms;

        // Gate window, with drag-to-select.
        if self.mode == Mode::Trigger {
            let resp = ui.interact(rect, ui.scope_id().with("scope_drag"), Sense::drag());
            let ms_at = |p: Pos2| ((p.x - rect.left()) / rect.width() * self.view_ms).clamp(0.0, self.view_ms);
            if resp.drag_started() {
                self.drag_from_ms = resp.interact_pointer_pos().map(ms_at);
            }
            if let (Some(a), Some(p)) = (self.drag_from_ms, resp.interact_pointer_pos()) {
                let b = ms_at(p);
                (self.win_start_ms, self.win_end_ms) = (a.min(b), a.max(b));
            }
            if resp.drag_stopped() {
                self.drag_from_ms = None;
            }
            let gate = Rect::from_x_y_ranges(x_of_ms(self.win_start_ms)..=x_of_ms(self.win_end_ms), rect.y_range());
            painter.rect_filled(gate, 0.0, theme::ACCENT.gamma_multiply(0.25));
            let y = |v: f32| rect.center().y - v * rect.height() / 2.0;
            for v in [self.threshold, -self.threshold] {
                let yy = y(v * self.gain);
                painter.hline(rect.x_range(), yy, Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.4)));
            }
        }
        painter.hline(rect.x_range(), rect.center().y, Stroke::new(1.0, Color32::from_gray(40)));

        // Min/max per pixel column.
        let cols = rect.width().max(1.0) as usize;
        let lo = start.saturating_sub(f.start) as usize;
        let half = rect.height() / 2.0;
        for c in 0..cols {
            let a = lo + c * view / cols;
            let b = (lo + (c + 1) * view / cols).max(a + 1);
            let Some(seg) = f.samples.get(a..b.min(f.samples.len())) else { break };
            if seg.is_empty() {
                break;
            }
            let (mn, mx) = seg.iter().fold((f32::MAX, f32::MIN), |(mn, mx), &s| (mn.min(s), mx.max(s)));
            let x = rect.left() + c as f32 + 0.5;
            let yc = rect.center().y;
            painter.vline(
                x,
                (yc - (mx * self.gain).clamp(-1.0, 1.0) * half)..=(yc - (mn * self.gain).clamp(-1.0, 1.0) * half),
                Stroke::new(1.0, theme::WAVE),
            );
        }
    }
}

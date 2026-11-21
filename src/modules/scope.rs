//! Trigger scope. In Trigger mode it locks onto an impulse (a kick) and
//! shows the audio after it; the shaded window (drag on the graph, or use
//! the sliders) becomes the gate the other modules analyse. Free mode just
//! scrolls, and the others analyse everything.

use serde::{Deserialize, Serialize};

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Ui};

use super::{Gate, Module, Shared};
use crate::ui::handle::EdgeHandle;
use crate::ui::plot::{self, Axis, Edge, Link, Scale};
use crate::ui::theme;

/// Sliders tied to spots on the graph.
#[derive(Clone, Copy, PartialEq)]
enum Key {
    View,
    Threshold,
    Holdoff,
    From,
    To,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Mode {
    Free,
    Trigger,
}

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
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
}

impl Default for Params {
    fn default() -> Self {
        Self {
            mode: Mode::Trigger,
            threshold: 0.3,
            holdoff_ms: 200.0,
            view_ms: 600.0,
            win_start_ms: 80.0,
            win_end_ms: 400.0,
            gain: 1.0,
        }
    }
}

impl Params {
    /// Pull loaded values back into the ranges the UI allows.
    fn sanitize(&mut self) {
        self.view_ms = self.view_ms.clamp(50.0, 2000.0);
        self.gain = self.gain.clamp(0.5, 16.0);
        self.threshold = self.threshold.clamp(0.01, 1.0);
        self.holdoff_ms = self.holdoff_ms.clamp(0.0, 1000.0);
        self.win_start_ms = self.win_start_ms.clamp(0.0, self.view_ms);
        self.win_end_ms = self.win_end_ms.clamp(self.win_start_ms, self.view_ms);
    }
}

#[derive(Default)]
pub struct Scope {
    p: Params,
    /// Absolute sample index of the trigger being shown.
    trigger: Option<u64>,
    drag_from_ms: Option<f32>,
    link: Link<Key>,
}

impl Scope {
    /// Latest rising crossing of the threshold that has a full gate window
    /// of audio after it and is past the holdoff of the previous one.
    fn find_trigger(&self, shared: &Shared) -> Option<u64> {
        let f = &shared.frame;
        let x = &f.samples;
        let need = f.ms_to_samples(self.p.win_end_ms.max(self.p.view_ms));
        let holdoff = f.ms_to_samples(self.p.holdoff_ms);
        let last = x.len().checked_sub(need)?;
        let mut found = None;
        let mut i = 1;
        while i <= last {
            if x[i].abs() >= self.p.threshold && x[i - 1].abs() < self.p.threshold {
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

    fn lookback(&self, rate: u32) -> usize {
        // Room to find a trigger with a full view after it, and to keep
        // showing the last one for a while.
        let ms = (2.0 * self.p.view_ms.max(self.p.win_end_ms) + self.p.holdoff_ms).max(2000.0);
        (ms * rate as f32 / 1000.0) as usize
    }

    fn update(&mut self, shared: &mut Shared) {
        match self.p.mode {
            Mode::Free => shared.gate = Gate { window: None },
            Mode::Trigger => {
                if let Some(t) = self.find_trigger(shared) {
                    self.trigger = Some(t);
                }
                let f = &shared.frame;
                shared.gate.window = self.trigger.map(|t| {
                    (t + f.ms_to_samples(self.p.win_start_ms) as u64, t + f.ms_to_samples(self.p.win_end_ms) as u64)
                });
            }
        }
    }

    fn params(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.p.mode, Mode::Trigger, "Trigger");
            ui.selectable_value(&mut self.p.mode, Mode::Free, "Free");
        });
        let link = &mut self.link;
        link.slider(ui, Key::View, egui::Slider::new(&mut self.p.view_ms, 50.0..=2000.0).text("view ms"));
        ui.add(egui::Slider::new(&mut self.p.gain, 0.5..=16.0).logarithmic(true).text("gain"));
        ui.add_enabled_ui(self.p.mode == Mode::Trigger, |ui| {
            link.slider(ui, Key::Threshold, egui::Slider::new(&mut self.p.threshold, 0.01..=1.0).text("threshold"));
            link.slider(ui, Key::Holdoff, egui::Slider::new(&mut self.p.holdoff_ms, 0.0..=1000.0).text("holdoff ms"));
            link.slider(ui, Key::From, egui::Slider::new(&mut self.p.win_start_ms, 0.0..=self.p.view_ms).text("gate from"));
            link.slider(ui, Key::To, egui::Slider::new(&mut self.p.win_end_ms, 0.0..=self.p.view_ms).text("gate to"));
        });
        if self.p.win_end_ms < self.p.win_start_ms {
            std::mem::swap(&mut self.p.win_start_ms, &mut self.p.win_end_ms);
        }
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        let f = &shared.frame;
        let trig = self.p.mode == Mode::Trigger;
        let view = f.ms_to_samples(self.p.view_ms).max(2);
        let start = match (trig, self.trigger) {
            (true, Some(t)) => t,
            _ => f.start + f.samples.len().saturating_sub(view) as u64,
        };
        let painter = ui.painter_at(rect);
        // Trigger: ms after the trigger. Free: ms before now.
        let xs = if trig {
            Scale::linear((0.0, self.p.view_ms), (rect.left(), rect.right()))
        } else {
            Scale::linear((-self.p.view_ms, 0.0), (rect.left(), rect.right()))
        };
        let x_of_ms = |ms: f32| rect.left() + rect.width() * ms / self.p.view_ms;
        let amp = Scale::linear((-1.0 / self.p.gain, 1.0 / self.p.gain), (rect.bottom(), rect.top()));

        // Gate window, with drag-to-select.
        if trig {
            let resp = ui.interact(rect, ui.scope_id().with("scope_drag"), Sense::drag());
            let ms_at = |p: Pos2| ((p.x - rect.left()) / rect.width() * self.p.view_ms).clamp(0.0, self.p.view_ms);
            if resp.drag_started() {
                self.drag_from_ms = resp.interact_pointer_pos().map(ms_at);
            }
            if let (Some(a), Some(p)) = (self.drag_from_ms, resp.interact_pointer_pos()) {
                let b = ms_at(p);
                (self.p.win_start_ms, self.p.win_end_ms) = (a.min(b), a.max(b));
            }
            if resp.drag_stopped() {
                self.drag_from_ms = None;
            }
            let gate = Rect::from_x_y_ranges(x_of_ms(self.p.win_start_ms)..=x_of_ms(self.p.win_end_ms), rect.y_range());
            painter.rect_filled(gate, 0.0, theme::ACCENT.gamma_multiply(0.25));
            for v in [self.p.threshold, -self.p.threshold] {
                painter.hline(rect.x_range(), amp.px(v), Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.4)));
            }
        }
        Axis::new(Edge::Bottom, xs).draw(&painter, rect, plot::fmt_ms);
        Axis::new(Edge::Left, amp).spacing(36.0).draw(&painter, rect, plot::trim);
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
                (yc - (mx * self.p.gain).clamp(-1.0, 1.0) * half)..=(yc - (mn * self.p.gain).clamp(-1.0, 1.0) * half),
                Stroke::new(1.0, theme::WAVE),
            );
        }

        // Handles: threshold on the left edge, gate ends along the bottom.
        if trig {
            let thr = Scale::linear((0.0, 1.0 / self.p.gain), (rect.center().y, rect.top()));
            let fmt = |v: f32| format!("thr {v:.2}  {:.1} dB", 20.0 * v.log10());
            let r = EdgeHandle::new("scope_thr", Edge::Left, thr, &mut self.p.threshold).clamp(0.01, 1.0).reset(0.3).label(&fmt).show(ui, rect);
            if r.hovered() || r.dragged() {
                self.link.touch(Key::Threshold);
            }
            let ms = Scale::linear((0.0, self.p.view_ms), (rect.left(), rect.right()));
            let fmt = |v: f32| format!("from {}", plot::fmt_ms(v));
            let r = EdgeHandle::new("scope_from", Edge::Bottom, ms, &mut self.p.win_start_ms)
                .clamp(0.0, self.p.win_end_ms)
                .label(&fmt)
                .show(ui, rect);
            if r.hovered() || r.dragged() {
                self.link.touch(Key::From);
            }
            let fmt = |v: f32| format!("to {}", plot::fmt_ms(v));
            let r = EdgeHandle::new("scope_to", Edge::Bottom, ms, &mut self.p.win_end_ms)
                .clamp(self.p.win_start_ms, self.p.view_ms)
                .label(&fmt)
                .show(ui, rect);
            if r.hovered() || r.dragged() {
                self.link.touch(Key::To);
            }
        }

        let caption = match (trig, self.trigger) {
            (true, Some(_)) => format!(
                "{} after trigger · gate {}–{}",
                plot::fmt_ms(self.p.view_ms),
                plot::fmt_ms(self.p.win_start_ms),
                plot::fmt_ms(self.p.win_end_ms)
            ),
            (true, None) => format!("{} · waiting for trigger", plot::fmt_ms(self.p.view_ms)),
            (false, _) => format!("last {}", plot::fmt_ms(self.p.view_ms)),
        };
        plot::caption(&painter, rect, Align2::RIGHT_TOP, caption);

        match self.link.shown() {
            Some(Key::View) => self.link.point_x(ui, rect, rect.right() - 1.0),
            Some(Key::Threshold) => self.link.point_y(ui, rect, amp.px(self.p.threshold)),
            Some(Key::Holdoff) => self.link.point_x(ui, rect, x_of_ms(self.p.holdoff_ms)),
            Some(Key::From) => self.link.point_x(ui, rect, x_of_ms(self.p.win_start_ms)),
            Some(Key::To) => self.link.point_x(ui, rect, x_of_ms(self.p.win_end_ms)),
            None => {}
        }
    }
}

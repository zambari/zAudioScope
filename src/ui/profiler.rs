//! Optional wall-clock timings for estimating UI-thread load, not process CPU.
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

/// Frames kept in the frame-time history graph.
const HISTORY: usize = 100;
/// Height of the frame-time history graph, in points.
const GRAPH_HEIGHT: f32 = 40.0;

#[derive(Default)]
pub struct Profiler {
    pub enabled: bool,
    /// Extra lines (ending in `\n`) shown under the timings, set per frame.
    pub note: String,
    rows: BTreeMap<(&'static str, &'static str), f64>,
    samples: BTreeMap<(&'static str, &'static str), f64>,
    previous: Option<Instant>,
    interval: f64,
    interval_bar: super::level_bar::Smoothed,
    /// Raw (unsmoothed) UI time of the last [`HISTORY`] frames, in seconds.
    history: VecDeque<f32>,
}

impl Profiler {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Default::default()
        }
    }

    pub fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub fn begin(&mut self) -> Option<Instant> {
        self.samples.clear();
        if !self.enabled {
            self.previous = None;
            self.interval = 0.0;
            self.rows.clear();
            self.interval_bar.reset();
            self.history.clear();
            return None;
        }
        let now = Instant::now();
        if let Some(previous) = self.previous.replace(now) {
            self.interval = now.duration_since(previous).as_secs_f64();
        }
        Some(now)
    }

    pub fn record(&mut self, group: &'static str, name: &'static str, start: Option<Instant>) {
        if let Some(start) = start {
            *self.samples.entry((group, name)).or_default() += start.elapsed().as_secs_f64();
        }
    }

    /// Records the frame total and paints the overlay with its top-right
    /// corner at `anchor`.
    pub fn finish(&mut self, ui: &egui::Ui, start: Option<Instant>, anchor: egui::Pos2) {
        self.record("Frame", "UI total", start);
        if !self.enabled {
            return;
        }
        if let Some(&frame) = self.samples.get(&("Frame", "UI total")) {
            if self.history.len() == HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(frame as f32);
        }
        self.rows.retain(|key, _| self.samples.contains_key(key));
        let alpha = super::level_bar::ema_alpha(self.interval as f32, 0.5) as f64;
        self.interval_bar
            .update((self.interval / 0.050) as f32, self.interval as f32);
        for (&key, &sample) in &self.samples {
            let average = self.rows.entry(key).or_insert(sample);
            *average += alpha * (sample - *average);
        }
        let total = self
            .rows
            .get(&("Frame", "UI total"))
            .copied()
            .unwrap_or_default();
        let load = if self.interval > 0.0 {
            total / self.interval * 100.0
        } else {
            0.0
        };
        let mut text = format!(
            "UI load ~{load:.1}% of one core · {:.2} ms/frame\nWall time; excludes GPU / audio callback\n",
            total * 1000.0
        );
        for (&(group, name), &seconds) in &self.rows {
            if group != "Frame" {
                text.push_str(&format!("{group:8} {name:14} {:7.0} µs\n", seconds * 1e6));
            }
        }
        text.push_str(&self.note);
        if self.interval > 0.0 {
            text.push_str(&format!(
                "Frame interval {:.1} / 50 ms · {:.1} FPS",
                self.interval * 1000.0,
                1.0 / self.interval,
            ));
        } else {
            text.push_str("Frame interval — / 50 ms");
        }
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::unique("debug_timings"),
        ));
        let galley =
            painter.layout_no_wrap(text, egui::FontId::monospace(11.0), egui::Color32::WHITE);
        let size = galley.size() + egui::vec2(16.0, 32.0 + GRAPH_HEIGHT);
        let rect = egui::Rect::from_min_size(anchor - egui::vec2(size.x, 0.0), size);
        painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(113));
        let text_pos = rect.min + egui::vec2(8.0, 6.0);
        let graph = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 8.0, text_pos.y + galley.size().y + 4.0),
            egui::vec2(rect.width() - 16.0, GRAPH_HEIGHT),
        );
        self.paint_history(&painter, graph, ui.visuals());
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.bottom() - 16.0),
            egui::pos2(rect.right() - 8.0, rect.bottom() - 6.0),
        );
        painter.rect_filled(bar, 1.0, ui.visuals().extreme_bg_color);
        self.interval_bar.paint(&painter, bar);
        painter.galley(text_pos, galley, egui::Color32::WHITE);
    }

    /// Block graph of the last [`HISTORY`] frame times, newest on the right.
    /// Scales to the slowest frame shown, never below the 60 FPS budget,
    /// which is marked with a line.
    fn paint_history(&self, painter: &egui::Painter, rect: egui::Rect, visuals: &egui::Visuals) {
        const BUDGET: f32 = 1.0 / 60.0;
        painter.rect_filled(rect, 1.0, visuals.extreme_bg_color);
        let peak = self.history.iter().copied().fold(BUDGET, f32::max);
        let width = rect.width() / HISTORY as f32;
        let skip = HISTORY - self.history.len();
        for (i, &seconds) in self.history.iter().enumerate() {
            let x = rect.left() + (skip + i) as f32 * width;
            let top = rect.bottom() - rect.height() * seconds / peak;
            let color = if seconds > BUDGET {
                egui::Color32::from_rgb(230, 90, 70)
            } else {
                egui::Color32::from_rgb(90, 200, 120)
            };
            let block = egui::Rect::from_min_max(
                egui::pos2(x, top),
                egui::pos2(x + (width - 1.0).max(1.0), rect.bottom()),
            );
            painter.rect_filled(block, 0.0, color);
        }
        let y = rect.bottom() - rect.height() * BUDGET / peak;
        painter.hline(
            rect.x_range(),
            y,
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90)),
        );
        painter.text(
            rect.right_top() + egui::vec2(-2.0, 1.0),
            egui::Align2::RIGHT_TOP,
            format!("{:.1} ms", peak * 1000.0),
            egui::FontId::monospace(9.0),
            egui::Color32::from_white_alpha(160),
        );
    }
}

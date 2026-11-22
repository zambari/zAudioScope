//! Optional wall-clock timings for estimating UI-thread load, not process CPU.
use std::{collections::BTreeMap, time::Instant};

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

    pub fn finish(&mut self, ui: &egui::Ui, start: Option<Instant>, origin: egui::Pos2) {
        self.record("Frame", "UI total", start);
        if !self.enabled {
            return;
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
        let rect = egui::Rect::from_min_size(origin, galley.size() + egui::vec2(16.0, 26.0));
        painter.rect_filled(rect, 4.0, egui::Color32::from_black_alpha(113));
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.bottom() - 16.0),
            egui::pos2(rect.right() - 8.0, rect.bottom() - 6.0),
        );
        painter.rect_filled(bar, 1.0, ui.visuals().extreme_bg_color);
        self.interval_bar.paint(&painter, bar);
        painter.galley(origin + egui::vec2(8.0, 6.0), galley, egui::Color32::WHITE);
    }
}

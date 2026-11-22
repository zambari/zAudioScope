//! Per-stage analysis costs and the "performance" foldout that shows them
//! above a module's speed / detail knobs.

use std::time::Instant;

use egui::Ui;

use crate::ui::widgets::Foldout;

/// Smoothed wall time of one analysis stage, µs.
#[derive(Default)]
pub struct Cost(f32);

impl Cost {
    /// Fold in one sample, µs (0 when the stage was skipped).
    fn add(&mut self, us: f32) {
        self.0 += (us - self.0) * 0.1;
    }
}

/// Stage times of one analysis run off the UI thread, µs per stage (0 when
/// skipped, `None` when the stage isn't part of this run). Sent back with
/// the result and folded in by [`Costs::absorb`].
#[derive(Default)]
pub struct Laps(Vec<Option<f32>>);

impl Laps {
    fn set(&mut self, i: usize, us: f32) {
        if self.0.len() <= i {
            self.0.resize(i + 1, None);
        }
        self.0[i] = Some(us);
    }

    /// Run `f` as stage `i`, timing it.
    pub fn time<T>(&mut self, i: usize, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        self.set(i, t.elapsed().as_secs_f32() * 1e6);
        out
    }

    /// Stage `i` was skipped this analysis.
    pub fn skip(&mut self, i: usize) {
        self.set(i, 0.0);
    }
}

/// Costs of a module's analysis stages (indexed by the caller's own stage
/// list) plus how often the analysis runs.
#[derive(Default)]
pub struct Costs {
    stages: Vec<Cost>,
    /// Smoothed seconds between analyses, and when the last one ran.
    interval: f32,
    last_run: Option<Instant>,
}

impl Costs {
    fn stage(&mut self, i: usize) -> &mut Cost {
        if self.stages.len() <= i {
            self.stages.resize_with(i + 1, Cost::default);
        }
        &mut self.stages[i]
    }

    /// Run `f` as stage `i`, timing it.
    pub fn time<T>(&mut self, i: usize, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        self.stage(i).add(t.elapsed().as_secs_f32() * 1e6);
        out
    }

    /// Stage `i` was skipped this analysis.
    pub fn skip(&mut self, i: usize) {
        self.stage(i).add(0.0);
    }

    /// Fold in stage times measured elsewhere (on a worker thread).
    pub fn absorb(&mut self, laps: &Laps) {
        for (i, &us) in laps.0.iter().enumerate() {
            if let Some(us) = us {
                self.stage(i).add(us);
            }
        }
    }

    /// One analysis started.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_run.replace(now) {
            let dt = now.duration_since(prev).as_secs_f32().min(1.0);
            self.interval += (dt - self.interval) * if self.interval == 0.0 { 1.0 } else { 0.1 };
        }
    }

    fn total_us(&self) -> f32 {
        self.stages.iter().map(|c| c.0).sum()
    }

    /// Share of one core spent analysing, %.
    fn load(&self) -> f32 {
        if self.interval > 0.0 {
            self.total_us() * 1e-4 / self.interval
        } else {
            0.0
        }
    }

    /// The "performance" foldout: load summary, per-stage µs (`names` in
    /// stage order), then `knobs`.
    pub fn foldout(&self, ui: &mut Ui, salt: &str, names: &[&str], knobs: impl FnOnce(&mut Ui)) {
        let summary = format!("~{:.1}% core · {:.0} µs", self.load(), self.total_us());
        Foldout::new(salt, "performance")
            .summary(summary)
            .show(ui, |ui| {
                let mut text: String = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        format!("{n:<9}{:5.0} µs\n", self.stages.get(i).map_or(0.0, |c| c.0))
                    })
                    .collect();
                text += &format!("every {:.0} ms", self.interval * 1000.0);
                ui.label(egui::RichText::new(text).monospace().size(10.0).weak());
                knobs(ui);
            });
    }
}

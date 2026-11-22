//! Constant-Q style spectrum over the shared note range: one windowed DFT
//! per display point on a decimated copy of the input, with a window that
//! follows the note (up to `max_cycles` periods). Split out of the tuner so
//! its detail / CPU knobs stand alone; in the stack the tuner draws on top
//! of it (see [`Module::stack_over`]).

use egui::{Mesh, Pos2, Rect, Shape, Stroke, Ui, pos2};
use serde::{Deserialize, Serialize};

use super::note_input::{FLOOR_DB, NoteInput, Want, ease, note_db};
use super::perf::Costs;
use super::{Module, Shared};
use crate::pitch;
use crate::ui::dynamic_range::DynamicRange;
use crate::ui::noterange::A4;
use crate::ui::plot::{self, Axis, Edge, Scale};
use crate::ui::theme;

pub const TITLE: &str = "Graph";

/// Cost stages, in `Costs` order (0 is decimation, see `NoteInput::next`).
const STAGES: [&str; 2] = ["decimate", "spectrum"];

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    db: DynamicRange,
    /// Longest stretch of input analysed.
    window_ms: f32,
    // ---- performance ----
    rate_hz: f32,
    /// Points per semitone.
    steps: usize,
    /// The window spans at most this many periods of each note.
    max_cycles: f32,
    /// Decimated rate is at least this many times the top note.
    oversample: f32,
    /// Below this input level (dB) nothing is analysed.
    gate_db: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            db: DynamicRange::default(),
            window_ms: 300.0,
            rate_hz: 50.0,
            steps: 12,
            max_cycles: 48.0,
            oversample: 10.0,
            gate_db: -90.0,
        }
    }
}

impl Params {
    fn sanitize(&mut self) {
        self.window_ms = self.window_ms.clamp(60.0, 1000.0);
        self.rate_hz = self.rate_hz.clamp(2.0, 60.0);
        self.steps = self.steps.clamp(1, 16);
        self.max_cycles = self.max_cycles.clamp(4.0, 64.0);
        self.oversample = self.oversample.clamp(3.0, 20.0);
        self.gate_db = self.gate_db.clamp(-140.0, -20.0);
        self.db.sanitize();
    }

    fn want(&self) -> Want {
        Want { window_ms: self.window_ms, rate_hz: self.rate_hz, oversample: self.oversample, a4: A4 }
    }
}

#[derive(Default)]
pub struct NoteSpectrum {
    p: Params,
    input: NoteInput,
    /// Latest analysis, dB per point across `input.analysed`.
    target: Vec<f32>,
    /// Eased display copy.
    shown: Vec<f32>,
    costs: Costs,
}

impl NoteSpectrum {
    fn points(&self) -> usize {
        self.input.analysed.count() * self.p.steps + 1
    }

    /// `full`: background, labelled note grid and dB axis; otherwise the curve.
    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared, full: bool) {
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        if self.shown.len() != self.target.len() {
            self.shown = self.target.clone();
        }
        let (up, down) = (ease(dt, 0.025), ease(dt, 0.2));
        for (d, &t) in self.shown.iter_mut().zip(&self.target) {
            *d += (t - *d) * if t > *d { up } else { down };
        }

        let painter = ui.painter_at(rect);
        let plot = if full { plot::note_plot(rect) } else { rect };
        let range = shared.range.get();
        let (bottom, top) = (self.p.db.range.floor, self.p.db.range.ceil);
        let db_scale = Scale::linear((bottom, top), (plot.bottom(), plot.top()));
        if full {
            painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
            plot::note_axis(&painter, rect, range, |_| false, false);
            Axis::new(Edge::Left, db_scale).spacing(28.0).minor(false).draw(&painter, plot, |db| format!("{db:.0}"));
        }
        if self.shown.len() < 2 {
            return;
        }
        // Placed by the range it was analysed over (the bar may have just moved).
        let x = range.scale(rect);
        let (lo, hi) = self.input.analysed.span();
        let count = self.shown.len();
        let pts: Vec<Pos2> = self
            .shown
            .iter()
            .enumerate()
            .map(|(k, &d)| pos2(x.px(lo + (hi - lo) * k as f32 / (count - 1) as f32), db_scale.px(d.clamp(bottom, top))))
            .collect();
        let mut mesh = Mesh::default();
        let fill_top = theme::WAVE.gamma_multiply(0.30);
        let fill_bot = theme::WAVE.gamma_multiply(0.02);
        for p in &pts {
            // Brighter fill where the curve is higher.
            let t = 1.0 - (p.y - plot.top()) / plot.height();
            mesh.colored_vertex(*p, fill_top.gamma_multiply(0.35 + 0.65 * t));
            mesh.colored_vertex(pos2(p.x, plot.bottom()), fill_bot);
        }
        for k in 0..count as u32 - 1 {
            let (a, b) = (2 * k, 2 * k + 2);
            mesh.add_triangle(a, a + 1, b);
            mesh.add_triangle(b, a + 1, b + 1);
        }
        painter.add(Shape::mesh(mesh));
        painter.add(Shape::line(pts, Stroke::new(1.2, theme::WAVE.gamma_multiply(0.85))));
    }
}

impl Module for NoteSpectrum {
    fn title(&self) -> &'static str {
        TITLE
    }

    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str) {
        eframe::set_value(storage, key, &self.p);
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        // First run after the split: take the spectrum settings the tuner
        // kept (same field names; the tuner's own are ignored).
        let legacy = key.replace(TITLE, super::tuner::TITLE);
        if let Some(p) = eframe::get_value::<Params>(storage, key).or_else(|| eframe::get_value(storage, &legacy)) {
            self.p = p;
            self.p.sanitize();
        }
    }

    fn graph_height(&self) -> f32 {
        230.0
    }

    fn lookback(&self, rate: u32) -> usize {
        self.p.want().lookback(rate)
    }

    fn update(&mut self, shared: &mut Shared) {
        self.p.db.sync(&shared.db);
        let Some(w) = self.input.next(shared, &self.p.want(), &mut self.costs) else { return };
        let points = self.points();
        if w.level_db < self.p.gate_db {
            // Keep the length so the display decays instead of resetting.
            self.costs.skip(1);
            self.target = vec![FLOOR_DB; points];
            return;
        }
        let (lo, hi) = self.input.analysed.span();
        let max_cycles = self.p.max_cycles;
        self.target = self.costs.time(1, || {
            (0..points)
                .map(|k| note_db(&w.y, w.rate, pitch::midi_to_freq(lo + (hi - lo) * k as f32 / (points - 1) as f32, A4), max_cycles))
                .collect()
        });
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        self.p.db.ui(ui, shared);
        ui.add(egui::Slider::new(&mut self.p.window_ms, 60.0..=1000.0).logarithmic(true).suffix(" ms").text("window"));
        let p = &mut self.p;
        self.costs.foldout(ui, "spectrum_perf", &STAGES, |ui| {
            ui.add(egui::Slider::new(&mut p.rate_hz, 2.0..=60.0).logarithmic(true).suffix(" Hz").text("update rate"))
                .on_hover_text("Free-running analyses per second (gated mode analyses once per window)");
            ui.add(egui::Slider::new(&mut p.steps, 1..=16).text("pts/semi")).on_hover_text("Spectrum resolution");
            ui.add(egui::Slider::new(&mut p.max_cycles, 4.0..=64.0).logarithmic(true).text("cycles"))
                .on_hover_text("Window length in periods: fewer is cheaper but blurs peaks");
            ui.add(egui::Slider::new(&mut p.oversample, 3.0..=20.0).text("oversample"))
                .on_hover_text("Decimated rate vs top note: lower decimates harder (cheaper)");
            ui.add(egui::Slider::new(&mut p.gate_db, -140.0..=-20.0).suffix(" dB").text("silence gate"))
                .on_hover_text("Below this input level nothing is analysed");
        });
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, true);
    }

    fn layer_depth(&self) -> Option<u8> {
        Some(1)
    }

    fn layer(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, false);
    }
}

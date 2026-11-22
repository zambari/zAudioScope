//! Trigger scope. In Trigger mode it locks onto an impulse (a kick) and
//! shows the audio after it; the shaded window (drag on the graph, or use
//! the sliders) becomes the gate the other modules analyse. Free mode just
//! scrolls, and the others analyse everything. Auto mode is Trigger with the
//! threshold set by [`AutoLevel`], falling back to Free when nothing hits.

use serde::{Deserialize, Serialize};

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Ui};

use super::{Gate, Module, Shared};
use crate::audio::{Frame, loudness};
use crate::ui::dynamic_range::{CeilCrawl, DynamicRange};
use crate::ui::handle::EdgeHandle;
use crate::ui::plot::{self, Axis, Edge, Link, Scale};
use crate::ui::theme;

/// Sliders tied to spots on the graph.
#[derive(Clone, Copy, PartialEq)]
enum Key {
    View,
    Threshold,
    Holdoff,
    Reach,
    From,
    To,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Mode {
    Free,
    Trigger,
    Auto,
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
    /// Auto mode: how much quieter than the last hit the next one may be
    /// and still trigger, dB.
    auto_reach_db: f32,
    /// Time shown after the trigger (or in total, in free mode).
    view_ms: f32,
    /// Gate: analyse from `win_start_ms` to `win_end_ms` after the trigger.
    win_start_ms: f32,
    win_end_ms: f32,
    /// Vertical view: the ceiling is the top edge (0 dB = full scale), the
    /// floor sits on the centre line; anything quieter folds to zero, so
    /// raising the floor zooms into the peaks.
    db: DynamicRange,
    /// Legacy linear zoom, read once and migrated to the ceiling.
    #[serde(skip_serializing)]
    gain: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            mode: Mode::Trigger,
            threshold: 0.3,
            holdoff_ms: 200.0,
            auto_reach_db: 6.0,
            view_ms: 600.0,
            win_start_ms: 80.0,
            win_end_ms: 400.0,
            db: DynamicRange::default(),
            gain: f32::NAN,
        }
    }
}

impl Params {
    /// Pull loaded values back into the ranges the UI allows.
    fn sanitize(&mut self) {
        self.view_ms = self.view_ms.clamp(50.0, 2000.0);
        if self.gain.is_finite() && self.gain > 0.0 {
            self.db.range.ceil = -20.0 * self.gain.log10();
            self.gain = f32::NAN;
        }
        self.db.sanitize();
        self.threshold = self.threshold.clamp(0.01, 1.0);
        self.holdoff_ms = self.holdoff_ms.clamp(0.0, 1000.0);
        self.auto_reach_db = self.auto_reach_db.clamp(0.0, 24.0);
        self.win_start_ms = self.win_start_ms.clamp(0.0, self.view_ms);
        self.win_end_ms = self.win_end_ms.clamp(self.win_start_ms, self.view_ms);
    }

    fn fold(&self) -> Fold {
        Fold {
            floor: loudness::lin(self.db.range.floor),
            top: loudness::lin(self.db.range.ceil),
        }
    }
}

/// Vertical amplitude mapping: |s| at the floor lands on the centre line,
/// the ceiling on the edge, and anything under the floor folds to zero.
#[derive(Clone, Copy)]
struct Fold {
    floor: f32,
    top: f32,
}

impl Fold {
    /// -1..1 of the half height, clamped.
    fn norm(&self, s: f32) -> f32 {
        ((s.abs() - self.floor) / (self.top - self.floor))
            .clamp(0.0, 1.0)
            .copysign(s)
    }

    /// Screen y for a sample value.
    fn y(&self, rect: Rect, s: f32) -> f32 {
        rect.center().y - self.norm(s) * rect.height() / 2.0
    }

    /// Effective linear magnification of the visible span.
    fn zoom(&self) -> f32 {
        1.0 / (self.top - self.floor)
    }
}

/// Auto threshold: `reach` dB under the held peak of the last hit, so a
/// slightly quieter next hit still fires. The hold decays while nothing
/// hits, lowering the threshold to catch quieter material, but never below
/// [`AVG_FACTOR`] × the average level: a transient has to stand out of the
/// signal around it, so steady tones and pads (peak ≈ 1.6 × mean) never fire
/// and the scope falls back to passthrough instead.
#[derive(Default)]
struct AutoLevel {
    /// Slow mean of |x|.
    avg: f32,
    /// Peak-hold of |x|, decaying.
    hold: f32,
    /// Absolute sample index already consumed.
    pos: u64,
}

/// Threshold floor relative to the mean |x| (≈ +9.5 dB).
const AVG_FACTOR: f32 = 3.0;
const AVG_TAU_S: f32 = 1.0;
const HOLD_DECAY_DB_S: f32 = 6.0;
/// Absolute floor, so hiss alone never triggers (-50 dBFS).
const AUTO_MIN: f32 = 0.003;
/// Without a fresh trigger for this long past the view, fall back to free.
const AUTO_STALE_MS: f32 = 1500.0;

impl AutoLevel {
    /// Consume the audio in `frame` not seen yet.
    fn update(&mut self, frame: &Frame, reset: bool) {
        if reset || self.pos > frame.end() {
            *self = Self::default();
        }
        let from = self.pos.max(frame.start);
        let new = &frame.samples[(from - frame.start) as usize..];
        self.pos = frame.end();
        let rate = frame.rate.max(1) as f32;
        let a = 1.0 - (-1.0 / (AVG_TAU_S * rate)).exp();
        let decay = loudness::lin(-HOLD_DECAY_DB_S / rate);
        for s in new.iter().map(|s| s.abs()) {
            self.avg += (s - self.avg) * a;
            self.hold = (self.hold * decay).max(s);
        }
    }

    fn threshold(&self, reach_db: f32) -> f32 {
        (self.hold * loudness::lin(-reach_db))
            .max(self.avg * AVG_FACTOR)
            .clamp(AUTO_MIN, 1.0)
    }
}

#[derive(Default)]
pub struct Scope {
    p: Params,
    /// Ceiling follower, active until the dB slider is first moved by hand.
    crawl: CeilCrawl,
    /// Absolute sample index of the trigger being shown.
    trigger: Option<u64>,
    auto: AutoLevel,
    drag_from_ms: Option<f32>,
    link: Link<Key>,
}

impl Scope {
    /// The level crossings are tested against: the slider, or the follower.
    fn threshold(&self) -> f32 {
        match self.p.mode {
            Mode::Auto => self.auto.threshold(self.p.auto_reach_db),
            _ => self.p.threshold,
        }
    }

    /// Showing time after a trigger (with the gate) rather than the latest
    /// audio. Trigger mode keeps that layout while waiting for the first hit;
    /// Auto only while it's locked.
    fn triggered(&self) -> bool {
        match self.p.mode {
            Mode::Free => false,
            Mode::Trigger => true,
            Mode::Auto => self.trigger.is_some(),
        }
    }

    /// Latest rising crossing of the threshold that has a full gate window
    /// of audio after it and is past the holdoff of the previous one.
    fn find_trigger(&self, shared: &Shared) -> Option<u64> {
        let f = &shared.frame;
        let x = &f.samples;
        let need = f.ms_to_samples(self.p.win_end_ms.max(self.p.view_ms));
        let holdoff = f.ms_to_samples(self.p.holdoff_ms);
        let thr = self.threshold();
        let last = x.len().checked_sub(need)?;
        let mut found = None;
        let mut i = 1;
        while i <= last {
            if x[i].abs() >= thr && x[i - 1].abs() < thr {
                found = Some(f.start + i as u64);
                i += holdoff.max(1);
            } else {
                i += 1;
            }
        }
        found
    }

    /// Sample offset into the frame and length of what's on screen: after
    /// the trigger, or the latest audio in free mode (or before one fires).
    fn span(&self, shared: &Shared) -> (usize, usize) {
        let f = &shared.frame;
        let view = f.ms_to_samples(self.p.view_ms).max(2);
        let start = match (self.p.mode, self.trigger) {
            (Mode::Trigger | Mode::Auto, Some(t)) => t.saturating_sub(f.start) as usize,
            _ => f.samples.len().saturating_sub(view),
        };
        (start, view)
    }

    /// Shaded gate window (triggered view only).
    fn paint_gate(&self, painter: &egui::Painter, rect: Rect) {
        if !self.triggered() {
            return;
        }
        let x_of_ms = |ms: f32| rect.left() + rect.width() * ms / self.p.view_ms;
        let gate = Rect::from_x_y_ranges(
            x_of_ms(self.p.win_start_ms)..=x_of_ms(self.p.win_end_ms),
            rect.y_range(),
        );
        painter.rect_filled(gate, 0.0, theme::ACCENT.gamma_multiply(0.25));
    }

    /// The trace: min/max per pixel column.
    fn paint_wave(&self, painter: &egui::Painter, rect: Rect, shared: &Shared) {
        let f = &shared.frame;
        let (lo, view) = self.span(shared);
        let fold = self.p.fold();
        let cols = rect.width().max(1.0) as usize;
        for c in 0..cols {
            let a = lo + c * view / cols;
            let b = (lo + (c + 1) * view / cols).max(a + 1);
            let Some(seg) = f.samples.get(a..b.min(f.samples.len())) else {
                break;
            };
            if seg.is_empty() {
                break;
            }
            let (mn, mx) = seg
                .iter()
                .fold((f32::MAX, f32::MIN), |(mn, mx), &s| (mn.min(s), mx.max(s)));
            let x = rect.left() + c as f32 + 0.5;
            painter.vline(
                x,
                fold.y(rect, mx)..=fold.y(rect, mn),
                Stroke::new(1.0, theme::WAVE),
            );
        }
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

    fn always_live(&self) -> bool {
        // Sets the gate every other analyser reads.
        true
    }

    fn update(&mut self, shared: &mut Shared) {
        self.p.db.sync(&shared.db);
        // Advance the follower even while locked so it never replays old audio,
        // but only write the ceiling back when this view owns its range.
        let mut range = self.p.db.range;
        self.crawl.update(&shared.frame, shared.reset, &mut range);
        if !self.p.db.locked {
            self.p.db.range = range;
        }
        self.auto.update(&shared.frame, shared.reset);
        match self.p.mode {
            Mode::Free => shared.gate = Gate { window: None },
            Mode::Trigger | Mode::Auto => {
                if let Some(t) = self.find_trigger(shared) {
                    self.trigger = Some(t);
                }
                let f = &shared.frame;
                if self.p.mode == Mode::Auto {
                    // Nothing hit lately (or the stream restarted): passthrough.
                    let stale = f
                        .ms_to_samples(self.p.view_ms.max(self.p.win_end_ms) + AUTO_STALE_MS)
                        as u64;
                    self.trigger = self
                        .trigger
                        .filter(|&t| t <= f.end() && f.end() - t <= stale);
                }
                shared.gate.window = self.trigger.map(|t| {
                    (
                        t + f.ms_to_samples(self.p.win_start_ms) as u64,
                        t + f.ms_to_samples(self.p.win_end_ms) as u64,
                    )
                });
            }
        }
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        if self.p.db.ui(ui, shared).changed() {
            self.crawl.manual = true;
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.p.mode, Mode::Trigger, "Trigger");
            ui.selectable_value(&mut self.p.mode, Mode::Auto, "Auto")
                .on_hover_text("Trigger with the threshold following the hits: a bit under the last one, never under the average level. Free when nothing stands out.");
            ui.selectable_value(&mut self.p.mode, Mode::Free, "Free");
        });
        let auto_thr = self.threshold();
        let link = &mut self.link;
        link.slider(
            ui,
            Key::View,
            egui::Slider::new(&mut self.p.view_ms, 50.0..=2000.0).text("view ms"),
        );
        ui.add_enabled_ui(self.p.mode != Mode::Free, |ui| {
            if self.p.mode == Mode::Auto {
                link.slider(
                    ui,
                    Key::Reach,
                    egui::Slider::new(&mut self.p.auto_reach_db, 0.0..=24.0).text("reach dB"),
                )
                .on_hover_text(
                    "How much quieter than the last hit the next one may be and still trigger.",
                );
                ui.label(format!("auto threshold {:.1} dBFS", loudness::db(auto_thr)));
            } else {
                link.slider(
                    ui,
                    Key::Threshold,
                    egui::Slider::new(&mut self.p.threshold, 0.01..=1.0).text("threshold"),
                );
            }
            link.slider(
                ui,
                Key::Holdoff,
                egui::Slider::new(&mut self.p.holdoff_ms, 0.0..=1000.0).text("holdoff ms"),
            );
            link.slider(
                ui,
                Key::From,
                egui::Slider::new(&mut self.p.win_start_ms, 0.0..=self.p.view_ms).text("gate from"),
            );
            link.slider(
                ui,
                Key::To,
                egui::Slider::new(&mut self.p.win_end_ms, 0.0..=self.p.view_ms).text("gate to"),
            );
        });
        if self.p.win_end_ms < self.p.win_start_ms {
            std::mem::swap(&mut self.p.win_start_ms, &mut self.p.win_end_ms);
        }
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        let f = &shared.frame;
        let trig = self.triggered();
        let (lo, view) = self.span(shared);
        let painter = ui.painter_at(rect);
        // Trigger: ms after the trigger. Free: ms before now.
        let xs = if trig {
            Scale::linear((0.0, self.p.view_ms), (rect.left(), rect.right()))
        } else {
            Scale::linear((-self.p.view_ms, 0.0), (rect.left(), rect.right()))
        };
        let x_of_ms = |ms: f32| rect.left() + rect.width() * ms / self.p.view_ms;
        let fold = self.p.fold();

        // Gate window, with drag-to-select.
        if trig {
            let resp = ui.interact(rect, ui.scope_id().with("scope_drag"), Sense::drag());
            let ms_at = |p: Pos2| {
                ((p.x - rect.left()) / rect.width() * self.p.view_ms).clamp(0.0, self.p.view_ms)
            };
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
        }
        self.paint_gate(&painter, rect);
        Axis::new(Edge::Bottom, xs).draw(&painter, rect, plot::fmt_ms);
        draw_db_axis(&painter, rect, fold);
        // Raised floor folds quiet signal onto the centre line: flag it orange.
        let centre = if self.p.db.range.floor_raised() {
            theme::ACCENT_BRIGHT
        } else {
            Color32::from_gray(40)
        };
        painter.hline(rect.x_range(), rect.center().y, Stroke::new(1.0, centre));

        self.paint_wave(&painter, rect, shared);

        // Auto threshold: a guide only, the follower owns it.
        if self.p.mode == Mode::Auto {
            let thr = self.threshold();
            let stroke = Stroke::new(
                1.0,
                theme::ACCENT.gamma_multiply(if trig { 0.8 } else { 0.35 }),
            );
            for s in [thr, -thr] {
                painter.hline(rect.x_range(), fold.y(rect, s), stroke);
            }
        }

        // Handles: threshold on the left edge, gate ends along the bottom.
        if trig {
            if self.p.mode == Mode::Trigger {
                let thr = Scale::linear((fold.floor, fold.top), (rect.center().y, rect.top()));
                let fmt = |v: f32| format!("thr {v:.2}  {:.1} dB", 20.0 * v.log10());
                let r = EdgeHandle::new("scope_thr", Edge::Left, thr, &mut self.p.threshold)
                    .clamp(0.01, 1.0)
                    .reset(0.3)
                    .label(&fmt)
                    .show(ui, rect);
                if r.hovered() || r.dragged() {
                    self.link.touch(Key::Threshold);
                }
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
            (false, _) if self.p.mode == Mode::Auto => format!(
                "last {} · auto: no hits, passthrough",
                plot::fmt_ms(self.p.view_ms)
            ),
            (false, _) => format!("last {}", plot::fmt_ms(self.p.view_ms)),
        };
        plot::caption(&painter, rect, Align2::RIGHT_TOP, caption);

        // Peak and loudness of what's on screen.
        let settle = f.ms_to_samples(100.0);
        if let Some((peak, lufs)) = loudness::measure(&f.samples, f.rate, lo, lo + view, settle) {
            let fmt = |v: f32| {
                if v.is_finite() {
                    format!("{v:.1}")
                } else {
                    "-inf".to_owned()
                }
            };
            let text = format!(
                "peak {} dBFS · {} LUFS · zoom ×{}",
                fmt(loudness::db(peak)),
                fmt(lufs),
                plot::trim(fold.zoom())
            );
            plot::caption(&painter, rect, Align2::CENTER_TOP, text);
        }

        match self.link.shown() {
            Some(Key::View) => self.link.point_x(ui, rect, rect.right() - 1.0),
            Some(Key::Threshold | Key::Reach) => {
                self.link.point_y(ui, rect, fold.y(rect, self.threshold()))
            }
            Some(Key::Holdoff) => self.link.point_x(ui, rect, x_of_ms(self.p.holdoff_ms)),
            Some(Key::From) => self.link.point_x(ui, rect, x_of_ms(self.p.win_start_ms)),
            Some(Key::To) => self.link.point_x(ui, rect, x_of_ms(self.p.win_end_ms)),
            None => {}
        }
    }

    /// Time runs across the panel, not pitch, so it gets a pane of its own
    /// under the composite layers rather than being stacked over them.
    fn composite_pane(&self) -> bool {
        true
    }
}

/// Left-edge amplitude ticks labelled in dBFS. The trace stays linear
/// (above the floor), so the ticks bunch towards the centre line; ones too
/// close to a kept one are dropped. Labels go on the upper half, ticks on both.
fn draw_db_axis(painter: &egui::Painter, rect: Rect, fold: Fold) {
    const DBS: [f32; 14] = [
        3.0, 0.0, -3.0, -6.0, -9.0, -12.0, -18.0, -24.0, -30.0, -36.0, -42.0, -48.0, -54.0, -60.0,
    ];
    const MIN_GAP: f32 = 14.0;
    let mut kept: Vec<f32> = Vec::new();
    for db in DBS {
        let v = loudness::lin(db);
        if v > fold.top * 1.0001 || v < fold.floor {
            continue;
        }
        let y = fold.y(rect, v);
        if (rect.center().y - y) < MIN_GAP || kept.iter().any(|&k| (k - y).abs() < MIN_GAP) {
            continue;
        }
        kept.push(y);
        let y_neg = fold.y(rect, -v);
        for yy in [y, y_neg] {
            painter.hline(rect.x_range(), yy, Stroke::new(1.0, plot::GRID));
            painter.line_segment(
                [
                    egui::pos2(rect.left(), yy),
                    egui::pos2(rect.left() + 5.0, yy),
                ],
                Stroke::new(1.0, plot::TICK),
            );
        }
        let unit = if kept.len() == 1 { " dBFS" } else { "" };
        let g = painter.layout_no_wrap(format!("{db:.0}{unit}"), plot::font(), plot::LABEL);
        let ty = if y - g.size().y < rect.top() + 1.0 {
            y + 1.0
        } else {
            y - g.size().y
        };
        painter.galley(egui::pos2(rect.left() + 7.0, ty), g, plot::LABEL);
    }
}

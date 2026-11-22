//! Pitch detector / tuner over the shared note range.
//!
//! - Autocorrelation (NSDF) candidates are drawn as vertical lines with
//!   note name and clarity; they fade in and out with confidence and level.
//! - Up to three clear candidates get a readout with the snapped note and
//!   the "tune up / down by x" advice, snapping to the nearest enabled note
//!   (or the nearest semitone when none are enabled). A note must stay clear
//!   for the confirm time before it shows and lingers for the hold time
//!   after it drops (see [`NoteHold`]), so letters don't flicker.
//! - The spectrum is its own module now; in the stack (and the tuner's solo
//!   page) the tuner draws on top of it when both are on.

use serde::{Deserialize, Serialize};

use egui::{Align2, Color32, FontId, Rect, Stroke, Ui, pos2, vec2};

use super::note_hold::{NoteHold, Reading, Timing};
use super::note_input::{NoteInput, Want, ease, note_db};
use super::perf::Costs;
use super::{Module, Shared};
use crate::pitch::{self, Method};
use crate::ui::icons::{self, Dir};
use crate::ui::noterange::NoteRange;
use crate::ui::plot::{self, Lanes, NOTE_LANE};
use crate::ui::theme;
use crate::ui::widgets::{self, Foldout};

pub const TITLE: &str = "Tuner";

/// Cost stages, in `Costs` order (0 is decimation, see `NoteInput::next`).
const STAGES: [&str; 3] = ["decimate", "nsdf", "ghosts"];

/// Candidates shown at most.
const MAX_CANDIDATES: usize = 6;
/// Below this NSDF clarity a candidate is ignored.
const MIN_CLARITY: f32 = 0.5;
/// A candidate within this many semitones of a track is that track.
const CAPTURE_SEMIS: f32 = 0.5;
/// A newcomer within this many semitones of an established track is
/// absorbed into it rather than spawning its own line.
const ABSORB_SEMIS: f32 = 1.2;
/// Time constant of a track's accumulated weight (≈ clarity at steady state).
const WEIGHT_TAU: f32 = 1.0;
/// A track is drawn only once its weight reaches this (≈0.4 s of steady 0.6 clarity).
const SHOW_WEIGHT: f32 = 0.2;
/// A candidate must be at least this clear to count towards a readout.
const SURE: f32 = 0.8;
/// A non-picked candidate whose spectral level is this far below the
/// loudest candidate's is a period-multiple (subharmonic) ghost of the
/// autocorrelation.
const GHOST_DB: f32 = 24.0;
/// Detection ceiling: a quarter tone above E6 (≈1357 Hz). NSDF costs
/// window × max lag, both proportional to the decimated rate, so cost goes
/// with the square of the top frequency. With the default 8× oversample this
/// is the highest semitone edge that still decimates by 4 at 44.1 and 48 kHz
/// (needs top ≤ rate/32): 16× cheaper than an undecimated wide range, where
/// a ~2 kHz cap would only decimate by 2 (4× cheaper). Fundamentals up to
/// the top of a 24-fret guitar; spectra and waterfalls keep the full range.
const TOP_MIDI: f32 = 88.5;
/// Periods of each candidate the ghost check's DFT looks at.
const GHOST_CYCLES: f32 = 24.0;

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    method: Method,
    /// 0 = no smoothing, towards 1 = very slow.
    smoothing: f32,
    /// A note must be clear this long before its readout shows.
    confirm_ms: f32,
    /// A readout lingers this long after its note drops out.
    hold_ms: f32,
    a4: f32,
    /// Device tune-control units per semitone; calibrate against the
    /// outboard gear ("0.06 per ~6 cents" → 1.0).
    units_per_semitone: f32,
    /// Snap-target toggles, indexed by MIDI note, 128 long.
    notes: Vec<bool>,
    /// Analysis window: longer resolves low notes better, shorter responds faster.
    window_ms: f32,
    // ---- performance ----
    /// Free-running analyses per second.
    rate_hz: f32,
    /// Decimated rate is at least this many times the top note.
    oversample: f32,
    /// Below this input level (dB) nothing is analysed.
    gate_db: f32,
    /// Legacy (fixed three-octave range), read once and migrated.
    #[serde(skip_serializing)]
    base_midi: i32,
    #[serde(skip_serializing)]
    enabled: Vec<bool>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            method: Method::Autocorrelation,
            smoothing: 0.8,
            confirm_ms: 150.0,
            hold_ms: 800.0,
            a4: 440.0,
            units_per_semitone: 1.0,
            notes: vec![false; 128],
            window_ms: 300.0,
            rate_hz: 50.0,
            oversample: 8.0,
            gate_db: -90.0,
            base_midi: -1,
            enabled: Vec::new(),
        }
    }
}

impl Params {
    /// Pull loaded values back into the ranges the UI allows.
    fn sanitize(&mut self) {
        self.smoothing = self.smoothing.clamp(0.0, 0.98);
        self.confirm_ms = self.confirm_ms.clamp(20.0, 1000.0);
        self.hold_ms = self.hold_ms.clamp(0.0, 3000.0);
        self.a4 = self.a4.clamp(415.0, 466.0);
        self.units_per_semitone = self.units_per_semitone.clamp(0.01, 10.0);
        self.notes.resize(128, false);
        if self.base_midi >= 0 {
            let base = self.base_midi.clamp(0, 127);
            for (i, &on) in self.enabled.iter().enumerate() {
                if let Some(n) = self.notes.get_mut(base as usize + i) {
                    *n = on;
                }
            }
            self.base_midi = -1;
            self.enabled.clear();
        }
        self.window_ms = self.window_ms.clamp(60.0, 1000.0);
        self.rate_hz = self.rate_hz.clamp(2.0, 60.0);
        self.oversample = self.oversample.clamp(3.0, 20.0);
        self.gate_db = self.gate_db.clamp(-140.0, -20.0);
    }

    fn want(&self) -> Want {
        Want {
            window_ms: self.window_ms,
            rate_hz: self.rate_hz,
            oversample: self.oversample,
            a4: self.a4,
            max_midi: TOP_MIDI,
        }
    }

    fn timing(&self) -> Timing {
        // `smoothing` is a per-analysis factor; as a time constant:
        let glide = if self.smoothing > 0.0 {
            -1.0 / (self.rate_hz * self.smoothing.ln())
        } else {
            0.0
        };
        Timing {
            confirm: self.confirm_ms / 1000.0,
            hold: self.hold_ms / 1000.0,
            glide,
        }
    }
}

/// A candidate line on screen, eased between analyses.
struct Track {
    midi: f32,
    clarity: f32,
    alpha: f32,
    /// 0..1, eased: how much this is the picked pitch.
    primary: f32,
    /// Matched a candidate this frame.
    live: bool,
    target_primary: f32,
    /// Leaky integral of matched clarity: persistence, decays when unmatched.
    weight: f32,
}

/// What [`Tuner::paint`] draws.
#[derive(Clone, Copy, PartialEq)]
enum Draw {
    /// Its own panel: background, note grid, labels and readout.
    Full,
    /// Over another note graph's full panel: everything but background and grid.
    Overlay,
    /// Composite layer: the pitch lines alone.
    Layer,
}

#[derive(Default)]
pub struct Tuner {
    p: Params,
    input: NoteInput,
    /// (midi, clarity, is the pick)
    raw: Vec<(f32, f32, bool)>,
    pick_clarity: f32,
    level_db: f32,
    /// Smoothed fractional MIDI note of the pick, `None` when silent.
    midi: Option<f32>,

    // Display state, eased per frame.
    tracks: Vec<Track>,
    readings: NoteHold,

    costs: Costs,
}

impl Tuner {
    fn enabled_notes(&self) -> Vec<i32> {
        let on: Vec<i32> = self
            .input
            .analysed
            .notes()
            .filter(|&m| self.is_enabled(m))
            .collect();
        if on.is_empty() {
            (0..128).collect()
        } else {
            on
        }
    }

    fn is_enabled(&self, midi: i32) -> bool {
        (0..128).contains(&midi) && self.p.notes[midi as usize]
    }
}

impl Module for Tuner {
    fn title(&self) -> &'static str {
        TITLE
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
        230.0
    }

    fn lookback(&self, rate: u32) -> usize {
        self.p.want().lookback(rate)
    }

    fn update(&mut self, shared: &mut Shared) {
        let want = self.p.want();
        let Some(w) = self.input.next(shared, &want, &mut self.costs) else {
            return;
        };
        let (y, r) = (&w.y, w.rate);
        self.level_db = w.level_db;
        let (lo, hi) = self.input.analysed.span();
        let hi = want.top(hi);
        let a4 = self.p.a4;
        let (f_lo, f_hi) = (
            pitch::midi_to_freq(lo - 0.5, a4),
            pitch::midi_to_freq(want.top(hi + 0.5), a4),
        );
        let c = if self.level_db < self.p.gate_db {
            self.costs.skip(1);
            pitch::Candidates::default()
        } else {
            self.costs.time(1, || pitch::candidates(y, r, f_lo, f_hi))
        };
        let mut raw: Vec<(f32, f32, bool)> = c
            .list
            .iter()
            .enumerate()
            .map(|(i, k)| {
                (
                    pitch::freq_to_midi(k.freq, a4),
                    k.clarity,
                    c.pick == Some(i),
                )
            })
            .filter(|&(m, cl, _)| cl >= MIN_CLARITY && m >= lo - 0.5 && m <= hi + 0.5)
            .collect();
        // NSDF also peaks at 2T, 3T… of the true period: lines with no
        // spectral energy under them are those subharmonic ghosts.
        self.costs.time(2, || {
            let level = |m: f32| {
                [-0.5, -0.25, 0.0, 0.25, 0.5]
                    .map(|d| note_db(y, r, pitch::midi_to_freq(m + d, a4), GHOST_CYCLES))
                    .into_iter()
                    .fold(f32::MIN, f32::max)
            };
            let levels: Vec<f32> = raw.iter().map(|&(m, ..)| level(m)).collect();
            let peak = levels.iter().copied().fold(f32::MIN, f32::max);
            let mut keep = levels.iter().map(|&l| l >= peak - GHOST_DB);
            raw.retain(|&(_, _, pick)| keep.next().unwrap() || pick);
        });
        raw.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.total_cmp(&a.1)));
        raw.truncate(MAX_CANDIDATES);
        self.raw = raw;

        let picked = c
            .pick
            .map(|i| (pitch::freq_to_midi(c.list[i].freq, a4), c.list[i].clarity));
        self.pick_clarity = picked.map_or(0.0, |p| p.1);
        self.midi = match (self.midi, picked.map(|p| p.0)) {
            // Jumped by more than a semitone: a new note, don't glide to it.
            (Some(prev), Some(m)) if (m - prev).abs() < 1.0 => {
                Some(prev * self.p.smoothing + m * (1.0 - self.p.smoothing))
            }
            (_, m) => m,
        };
    }

    fn marks(&self, out: &mut Vec<f32>) {
        if let (Some(m), true) = (self.midi, self.pick_clarity >= SURE) {
            out.push(m);
        }
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        egui::ComboBox::from_id_salt("tuner_method")
            .selected_text(self.p.method.label())
            .show_ui(ui, |ui| {
                for m in Method::ALL {
                    ui.selectable_value(&mut self.p.method, m, m.label());
                }
            });
        ui.add(egui::Slider::new(&mut self.p.smoothing, 0.0..=0.98).text("smoothing"));
        ui.add(
            egui::Slider::new(&mut self.p.confirm_ms, 20.0..=1000.0)
                .logarithmic(true)
                .suffix(" ms")
                .text("confirm"),
        )
        .on_hover_text("A note must stay clear this long before its readout shows");
        ui.add(
            egui::Slider::new(&mut self.p.hold_ms, 0.0..=3000.0)
                .suffix(" ms")
                .text("hold"),
        )
        .on_hover_text("A readout lingers this long after its note drops out");
        ui.add(
            egui::Slider::new(&mut self.p.window_ms, 60.0..=1000.0)
                .logarithmic(true)
                .suffix(" ms")
                .text("window"),
        );
        ui.add(egui::Slider::new(&mut self.p.a4, 415.0..=466.0).text("A4 Hz"));
        ui.add(
            egui::Slider::new(&mut self.p.units_per_semitone, 0.01..=10.0)
                .logarithmic(true)
                .text("units/semi"),
        );
        let p = &mut self.p;
        self.costs.foldout(ui, "tuner_perf", &STAGES, |ui| {
            ui.add(egui::Slider::new(&mut p.rate_hz, 2.0..=60.0).logarithmic(true).suffix(" Hz").text("update rate"))
                .on_hover_text("Free-running analyses per second (gated mode analyses once per window)");
            ui.add(egui::Slider::new(&mut p.oversample, 3.0..=20.0).text("oversample"))
                .on_hover_text("Decimated rate vs top note: lower decimates harder (cheaper), coarser NSDF lags");
            ui.add(egui::Slider::new(&mut p.gate_db, -140.0..=-20.0).suffix(" dB").text("silence gate"))
                .on_hover_text("Below this input level NSDF is skipped");
        });
        let NoteRange { lo, hi, .. } = shared.range.get();
        let count = (lo..=hi).filter(|&m| self.is_enabled(m)).count();
        let summary = if count == 0 {
            "any semitone".to_owned()
        } else {
            format!("{count} toggled notes")
        };
        Foldout::new("snap", "snap to")
            .summary(summary)
            .show(ui, |ui| {
                widgets::clear_notes(ui, &mut self.p.notes);
                // One row per octave in range, one tiny toggle per note.
                ui.spacing_mut().item_spacing = vec2(1.0, 1.0);
                for oct in lo.div_euclid(12)..=hi.div_euclid(12) {
                    let active = |m: i32| (lo..=hi).contains(&m);
                    widgets::note_row(ui, oct * 12, Some(oct - 1), &mut self.p.notes, active);
                }
            });
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, Draw::Full);
    }

    fn stack_over(&self) -> Option<&'static str> {
        Some(super::note_spectrum::TITLE)
    }

    fn overlay(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, Draw::Overlay);
    }

    fn layer_depth(&self) -> Option<u8> {
        Some(2)
    }

    fn layer(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, Draw::Layer);
    }
}

impl Tuner {
    /// Ease the candidate tracks and readout towards the latest analysis.
    fn animate(&mut self, dt: f32) {
        let level = ((self.level_db + 62.0) / 30.0).clamp(0.0, 1.0);
        for t in &mut self.tracks {
            t.live = false;
            t.target_primary = 0.0;
        }
        for &(m, cl, pick) in &self.raw {
            let w = cl * dt / WEIGHT_TAU;
            let dist = |t: &Track| (t.midi - m).abs();
            // Same note as an existing track: follow it closely.
            let near = (0..self.tracks.len())
                .filter(|&i| dist(&self.tracks[i]) < CAPTURE_SEMIS)
                .min_by(|&a, &b| dist(&self.tracks[a]).total_cmp(&dist(&self.tracks[b])));
            // Otherwise, a nearby established track soaks it up, pulled
            // towards it only in proportion to the power it adds.
            let host = near.or_else(|| {
                (0..self.tracks.len())
                    .filter(|&i| {
                        dist(&self.tracks[i]) < ABSORB_SEMIS && self.tracks[i].weight >= SHOW_WEIGHT
                    })
                    .max_by(|&a, &b| self.tracks[a].weight.total_cmp(&self.tracks[b].weight))
            });
            let t = match host {
                Some(i) => &mut self.tracks[i],
                None => {
                    self.tracks.push(Track {
                        midi: m,
                        clarity: cl,
                        alpha: 0.0,
                        primary: 0.0,
                        live: false,
                        target_primary: 0.0,
                        weight: 0.0,
                    });
                    self.tracks.last_mut().unwrap()
                }
            };
            if near.is_some() || t.weight == 0.0 {
                t.midi += (m - t.midi) * ease(dt, 0.06);
                t.clarity += (cl - t.clarity) * ease(dt, 0.1);
            } else {
                t.midi += (m - t.midi) * w / (t.weight + w);
            }
            t.weight += w;
            if pick {
                t.target_primary = 1.0;
            }
            t.live = true;
        }
        // Tracks that drifted together merge, the weaker into the stronger.
        self.tracks.sort_by(|a, b| b.weight.total_cmp(&a.weight));
        let mut i = 0;
        while i < self.tracks.len() {
            let mut j = i + 1;
            while j < self.tracks.len() {
                if (self.tracks[j].midi - self.tracks[i].midi).abs() < CAPTURE_SEMIS {
                    let b = self.tracks.remove(j);
                    let a = &mut self.tracks[i];
                    let total = (a.weight + b.weight).max(1e-6);
                    a.midi += (b.midi - a.midi) * b.weight / total;
                    a.weight += b.weight;
                    a.alpha = a.alpha.max(b.alpha);
                    a.live |= b.live;
                    a.target_primary = a.target_primary.max(b.target_primary);
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
        let decay = 1.0 - ease(dt, WEIGHT_TAU);
        for t in &mut self.tracks {
            t.weight *= decay;
            let shown = t.live && t.weight >= SHOW_WEIGHT;
            let target = if shown {
                ((t.clarity - 0.4) / 0.6).clamp(0.0, 1.0).max(0.25) * level
            } else {
                0.0
            };
            t.alpha += (target - t.alpha)
                * if target > t.alpha {
                    ease(dt, 0.12)
                } else {
                    ease(dt, 0.45)
                };
            t.primary += (t.target_primary - t.primary) * ease(dt, 0.15);
        }
        self.tracks.retain(|t| t.weight > 0.01 || t.alpha > 0.004);


        let clear: Vec<(f32, f32)> = self
            .raw
            .iter()
            .filter(|&&(_, cl, _)| cl >= SURE)
            .map(|&(m, cl, _)| (m, cl))
            .collect();
        let targets = self.enabled_notes();
        self.readings.step(dt, &clear, &targets, &self.p.timing());
    }

    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared, draw: Draw) {
        self.animate(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        let painter = ui.painter_at(rect);
        let range = shared.range.get();
        let labelled = draw != Draw::Layer;
        let plot = if labelled {
            plot::note_plot(rect)
        } else {
            rect
        };
        let x_of = |m: f32| range.scale(rect).px(m);
        match draw {
            Draw::Full => {
                painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
                plot::note_axis(&painter, rect, range, |m| self.is_enabled(m), false);
            }
            Draw::Overlay => plot::note_axis(&painter, rect, range, |m| self.is_enabled(m), true),
            Draw::Layer => {}
        }

        // Target notes of the readouts, faint, under the lines.
        for r in self.readings.visible() {
            painter.vline(
                x_of(r.note as f32),
                plot.y_range(),
                Stroke::new(2.0, theme::GOOD.gamma_multiply(0.25 * r.alpha)),
            );
        }

        // ---- candidates ---------------------------------------------------
        let mut order: Vec<usize> = (0..self.tracks.len()).collect();
        order.sort_by(|&a, &b| self.tracks[a].midi.total_cmp(&self.tracks[b].midi));
        let mut lanes = Lanes::new(2, 4.0);
        for &i in &order {
            let t = &self.tracks[i];
            if t.alpha < 0.01 {
                continue;
            }
            let x = x_of(t.midi);
            let base = theme::lerp(
                Color32::from_rgb(196, 176, 150),
                theme::ACCENT_BRIGHT,
                t.primary,
            );
            let col = base.gamma_multiply(t.alpha);
            painter.vline(x, plot.y_range(), Stroke::new(1.0 + 0.8 * t.primary, col));
            if !labelled {
                continue;
            }
            let text = format!(
                "{} {:.0}%",
                pitch::note_name(t.midi.round() as i32),
                t.clarity * 100.0
            );
            let galley = painter.layout_no_wrap(text, FontId::monospace(10.0), col);
            let w = galley.size().x;
            let left = (x - w / 2.0).clamp(rect.left() + 1.0, rect.right() - w - 1.0);
            let row = lanes.place(left, w);
            painter.galley(
                pos2(left, rect.top() + 2.0 + row as f32 * NOTE_LANE),
                galley,
                col,
            );
        }

        // ---- readouts ----------------------------------------------------
        if labelled {
            let hold = self.p.hold_ms / 1000.0;
            for r in self.readings.visible() {
                self.paint_reading(&painter, plot, r, hold);
            }
        }
    }

    /// One readout panel, stacked top-right by its eased slot; slot 0 is big.
    fn paint_reading(&self, painter: &egui::Painter, plot: Rect, r: &Reading, hold: f32) {
        const W: f32 = 176.0;
        const BIG: f32 = 64.0;
        const SMALL: f32 = 36.0;
        const GAP: f32 = 4.0;
        let big = r.slot < 0.5;
        let y = r.slot.min(1.0) * (BIG + GAP) + (r.slot - 1.0).max(0.0) * (SMALL + GAP);
        let panel = Rect::from_min_size(
            pos2(plot.right() - W - 8.0, plot.top() + 6.0 + y),
            vec2(W, if big { BIG } else { SMALL }),
        );
        let a = r.alpha;
        let cents = r.cents();
        let in_tune = cents.abs() < 3.0;
        let col = if in_tune {
            theme::GOOD
        } else {
            theme::ACCENT_BRIGHT
        }
        .gamma_multiply(a);
        let dim = Color32::from_gray(170).gamma_multiply(a);
        let units = cents / 100.0 * self.p.units_per_semitone;
        let (advice, arrow) = if in_tune {
            ("in tune".to_owned(), None)
        } else if cents > 0.0 {
            (format!("down {:.2}", units.abs()), Some(Dir::Down))
        } else {
            (format!("up {:.2}", units.abs()), Some(Dir::Up))
        };
        let hz = pitch::midi_to_freq(r.midi, self.p.a4);
        let (name_pt, advice_pt, detail_y) = if big {
            (26.0, 14.0, 24.0)
        } else {
            (18.0, 12.0, 20.0)
        };

        painter.rect_filled(panel, 2.0, Color32::from_black_alpha((150.0 * a) as u8));
        // Remaining hold, a shrinking line along the top once the note drops.
        if !r.live {
            let left = r.hold_left(hold);
            painter.hline(
                panel.left()..=panel.left() + panel.width() * left,
                panel.top() + 0.5,
                Stroke::new(1.0, dim),
            );
        }
        painter.text(
            panel.left_top() + vec2(8.0, 4.0),
            Align2::LEFT_TOP,
            pitch::note_name(r.note),
            FontId::proportional(name_pt),
            col,
        );
        let advice_rect = painter.text(
            panel.right_top() + vec2(-8.0, 6.0),
            Align2::RIGHT_TOP,
            &advice,
            FontId::proportional(advice_pt),
            col,
        );
        if let Some(dir) = arrow {
            let c = pos2(advice_rect.left() - 9.0, advice_rect.center().y);
            icons::triangle(painter, c, dir, advice_pt * 0.32, col);
        }
        painter.text(
            panel.right_top() + vec2(-8.0, detail_y),
            Align2::RIGHT_TOP,
            format!("{cents:+.1}¢  {hz:.2} Hz"),
            FontId::monospace(10.0),
            dim,
        );
        if !big {
            return;
        }
        // ±50 cent strip.
        let strip =
            Rect::from_min_size(panel.left_bottom() + vec2(8.0, -14.0), vec2(W - 16.0, 6.0));
        painter.hline(
            strip.x_range(),
            strip.center().y,
            Stroke::new(1.0, Color32::from_gray(70).gamma_multiply(a)),
        );
        painter.vline(
            strip.center().x,
            strip.y_range(),
            Stroke::new(1.0, theme::GOOD.gamma_multiply(0.6 * a)),
        );
        let mx = strip.center().x + (cents / 50.0).clamp(-1.0, 1.0) * strip.width() / 2.0;
        painter.circle_filled(pos2(mx, strip.center().y), 3.0, col);
    }
}

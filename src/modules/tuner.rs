//! Pitch detector / tuner, drawn as a spectral view over the note range.
//!
//! - A constant-Q style spectrum (one windowed DFT per display point, on a
//!   decimated copy of the input) spans the three octaves of note toggles,
//!   with a faint semitone grid behind it.
//! - Autocorrelation (NSDF) candidates are overlaid as vertical lines with
//!   note name and clarity; they fade in and out with confidence and level.
//! - When the pick is clearly periodic, the snapped note and the "tune up /
//!   down by x" advice fade in, snapping to the nearest enabled note (or the
//!   nearest semitone when none are enabled).

use serde::{Deserialize, Serialize};

use egui::{Align2, Color32, FontId, Mesh, Pos2, Rect, Shape, Stroke, Ui, pos2, vec2};

use super::{Module, Shared};
use crate::pitch::{self, Method};
use crate::ui::plot::{Axis, Edge, Lanes, Scale};
use crate::ui::theme;

const OCTAVES: usize = 3;
const NOTES: usize = 12 * OCTAVES;
/// Spectrum points per semitone.
const STEPS: usize = 12;
/// Free-running: one analysis per this much new audio.
const DETECT_MS: f32 = 20.0;
/// High notes need no more than this many periods of window.
const MAX_CYCLES: f32 = 48.0;
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
/// The pick must be at least this clear for the note readout to show.
const SURE: f32 = 0.8;

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    method: Method,
    /// 0 = no smoothing, towards 1 = very slow.
    smoothing: f32,
    a4: f32,
    /// Device tune-control units per semitone; calibrate against the
    /// outboard gear ("0.06 per ~6 cents" → 1.0).
    units_per_semitone: f32,
    /// MIDI note of the lowest toggle (C1 = 24).
    base_midi: i32,
    /// One per note from `base_midi`, `NOTES` long.
    enabled: Vec<bool>,
    /// Analysis window: longer resolves low notes better, shorter responds faster.
    window_ms: f32,
    /// Top of the spectrum view (fixed, no auto-follow).
    top_db: f32,
    /// Vertical span of the spectrum below its top.
    range_db: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            method: Method::Autocorrelation,
            smoothing: 0.8,
            a4: 440.0,
            units_per_semitone: 1.0,
            base_midi: 24,
            enabled: vec![false; NOTES],
            window_ms: 300.0,
            top_db: -48.0,
            range_db: 60.0,
        }
    }
}

impl Params {
    /// Pull loaded values back into the ranges the UI allows.
    fn sanitize(&mut self) {
        self.smoothing = self.smoothing.clamp(0.0, 0.98);
        self.a4 = self.a4.clamp(415.0, 466.0);
        self.units_per_semitone = self.units_per_semitone.clamp(0.01, 10.0);
        self.base_midi = self.base_midi.clamp(12, 84);
        self.enabled.resize(NOTES, false);
        self.window_ms = self.window_ms.clamp(60.0, 1000.0);
        self.top_db = self.top_db.clamp(-90.0, 6.0);
        self.range_db = self.range_db.clamp(24.0, 96.0);
    }

    /// Displayed range in fractional MIDI notes: half a semitone either side
    /// of the toggled notes.
    fn span(&self) -> (f32, f32) {
        (self.base_midi as f32 - 0.5, (self.base_midi + NOTES as i32) as f32 - 0.5)
    }
}

/// Low-pass + downsample so the spectrum and NSDF run on a few thousand
/// samples per second instead of 48k.
struct Decimator {
    rate: u32,
    factor: usize,
    taps: Vec<f32>,
}

impl Decimator {
    /// Largest power-of-two factor (≤ 32) that keeps the new rate at least
    /// ten times the top frequency, so NSDF lags stay well resolved.
    fn factor_for(rate: u32, f_top: f32) -> usize {
        let mut factor = 1;
        while factor < 32 && rate as f32 / (2 * factor) as f32 >= 10.0 * f_top {
            factor *= 2;
        }
        factor
    }

    fn new(rate: u32, f_top: f32) -> Self {
        let factor = Self::factor_for(rate, f_top);
        // Blackman windowed sinc, cutoff at the new Nyquist, transition
        // about 0.2 of the new rate: content that aliases lands above 0.4.
        let len = if factor == 1 { 1 } else { 28 * factor + 1 };
        let mid = (len / 2) as f32;
        let fc = 0.5 / factor as f32;
        let mut taps: Vec<f32> = (0..len)
            .map(|i| {
                if len == 1 {
                    return 1.0;
                }
                let t = i as f32 - mid;
                let sinc = if t == 0.0 { 2.0 * fc } else { (std::f32::consts::TAU * fc * t).sin() / (std::f32::consts::PI * t) };
                let u = i as f32 / (len - 1) as f32;
                let w = 0.42 - 0.5 * (std::f32::consts::TAU * u).cos() + 0.08 * (2.0 * std::f32::consts::TAU * u).cos();
                sinc * w
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self { rate, factor, taps }
    }

    fn out_rate(&self) -> u32 {
        self.rate / self.factor as u32
    }

    /// Filtered, decimated `x`, aligned to its newest fully-supported sample.
    fn run(&self, x: &[f32]) -> Vec<f32> {
        if self.factor == 1 {
            return x.to_vec();
        }
        let len = self.taps.len();
        if x.len() < len {
            return Vec::new();
        }
        let last = x.len() - len; // start of the newest full filter span
        let count = last / self.factor + 1;
        let first = last - (count - 1) * self.factor;
        (0..count)
            .map(|j| {
                let s = first + j * self.factor;
                x[s..s + len].iter().zip(&self.taps).map(|(a, b)| a * b).sum()
            })
            .collect()
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

#[derive(Default)]
pub struct Tuner {
    p: Params,
    decim: Option<Decimator>,
    /// Latest analysis.
    spec_target: Vec<f32>,
    /// (midi, clarity, is the pick)
    raw: Vec<(f32, f32, bool)>,
    pick_clarity: f32,
    level_db: f32,
    /// Smoothed fractional MIDI note of the pick, `None` when silent.
    midi: Option<f32>,
    /// What was analysed last: the gate window, or the stream position in
    /// free mode. Unchanged input is not analysed (or smoothed) again.
    last: Option<(u64, u64)>,

    // Display state, eased per frame.
    spec: Vec<f32>,
    tracks: Vec<Track>,
    readout: f32,
    shown_midi: f32,
}

impl Tuner {
    fn enabled_notes(&self) -> Vec<i32> {
        let on: Vec<i32> =
            (0..self.p.enabled.len()).filter(|&i| self.p.enabled[i]).map(|i| self.p.base_midi + i as i32).collect();
        if on.is_empty() { (0..128).collect() } else { on }
    }

    fn is_enabled(&self, midi: i32) -> bool {
        let i = midi - self.p.base_midi;
        i >= 0 && (i as usize) < NOTES && self.p.enabled[i as usize]
    }

    /// Hann-windowed DFT magnitude (dB re full-scale sine) at each display
    /// point. Window length follows the frequency (up to `MAX_CYCLES`
    /// periods), capped by what's available.
    fn spectrum(&self, y: &[f32], rate: u32) -> Vec<f32> {
        let (lo, hi) = self.p.span();
        let points = NOTES * STEPS + 1;
        (0..points)
            .map(|k| {
                let m = lo + (hi - lo) * k as f32 / (points - 1) as f32;
                let f = pitch::midi_to_freq(m, self.p.a4) as f64;
                let l = ((MAX_CYCLES as f64 * rate as f64 / f) as usize).min(y.len());
                if l < 16 {
                    return spectrum_floor();
                }
                let x = &y[y.len() - l..];
                // Phasor and window by rotation, no trig per sample.
                let w = std::f64::consts::TAU * f / rate as f64;
                let (wc, ws) = (w.cos(), w.sin());
                let h = std::f64::consts::TAU / l as f64;
                let (hc, hs) = (h.cos(), h.sin());
                let (mut pr, mut pi) = (1.0f64, 0.0f64);
                let (mut qr, mut qi) = (1.0f64, 0.0f64);
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for &s in x {
                    let v = s as f64 * (0.5 - 0.5 * qr);
                    re += v * pr;
                    im -= v * pi;
                    (pr, pi) = (pr * wc - pi * ws, pr * ws + pi * wc);
                    (qr, qi) = (qr * hc - qi * hs, qr * hs + qi * hc);
                }
                // A sine of amplitude A peaks at A·L/4 through a Hann window.
                let amp = (re * re + im * im).sqrt() * 4.0 / l as f64;
                (20.0 * amp.max(1e-9).log10()) as f32
            })
            .collect()
    }
}

fn spectrum_floor() -> f32 {
    -140.0
}

/// Exponential approach for a frame of `dt` seconds with time constant `tau`.
fn ease(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt / tau.max(1e-4)).exp()
}

impl Module for Tuner {
    fn title(&self) -> &'static str {
        "Tuner"
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
        // Window plus the decimation filter's span.
        (self.p.window_ms / 1000.0 * rate as f32) as usize + 32 * 28 + 1
    }

    fn update(&mut self, shared: &mut Shared) {
        let f = &shared.frame;
        let rate = f.rate;
        let key = match shared.gate.window {
            Some(w) => w,
            None => {
                let end = f.end();
                let hop = f.ms_to_samples(DETECT_MS) as u64;
                match self.last {
                    Some((0, t)) if !shared.reset && end >= t && end - t < hop => return,
                    _ => (0, end),
                }
            }
        };
        if self.last == Some(key) && !shared.reset {
            return;
        }
        self.last = Some(key);

        let (lo, hi) = self.p.span();
        let (f_lo, f_hi) = (pitch::midi_to_freq(lo - 0.5, self.p.a4), pitch::midi_to_freq(hi + 0.5, self.p.a4));
        if !self.decim.as_ref().is_some_and(|d| d.rate == rate && d.factor == Decimator::factor_for(rate, f_hi)) {
            self.decim = Some(Decimator::new(rate, f_hi));
        }
        let d = self.decim.as_ref().unwrap();
        let window = (self.p.window_ms / 1000.0 * rate as f32) as usize;
        let x = shared.analysis_slice(self.lookback(rate));
        let y = d.run(&x[x.len().saturating_sub(window + d.taps.len())..]);
        let r = d.out_rate();
        let y = &y[y.len().saturating_sub((self.p.window_ms / 1000.0 * r as f32) as usize)..];

        let rms = (y.iter().map(|s| s * s).sum::<f32>() / y.len().max(1) as f32).sqrt();
        self.level_db = 20.0 * rms.max(1e-9).log10();
        self.spec_target = self.spectrum(y, r);

        let c = pitch::candidates(y, r, f_lo, f_hi);
        let mut raw: Vec<(f32, f32, bool)> = c
            .list
            .iter()
            .enumerate()
            .map(|(i, k)| (pitch::freq_to_midi(k.freq, self.p.a4), k.clarity, c.pick == Some(i)))
            .filter(|&(m, cl, _)| cl >= MIN_CLARITY && m >= lo - 0.5 && m <= hi + 0.5)
            .collect();
        raw.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.total_cmp(&a.1)));
        raw.truncate(MAX_CANDIDATES);
        self.raw = raw;

        let picked = c.pick.map(|i| (pitch::freq_to_midi(c.list[i].freq, self.p.a4), c.list[i].clarity));
        self.pick_clarity = picked.map_or(0.0, |p| p.1);
        self.midi = match (self.midi, picked.map(|p| p.0)) {
            // Jumped by more than a semitone: a new note, don't glide to it.
            (Some(prev), Some(m)) if (m - prev).abs() < 1.0 => Some(prev * self.p.smoothing + m * (1.0 - self.p.smoothing)),
            (_, m) => m,
        };
    }

    fn params(&mut self, ui: &mut Ui) {
        egui::ComboBox::from_id_salt("tuner_method").selected_text(self.p.method.label()).show_ui(ui, |ui| {
            for m in Method::ALL {
                ui.selectable_value(&mut self.p.method, m, m.label());
            }
        });
        ui.add(egui::Slider::new(&mut self.p.smoothing, 0.0..=0.98).text("smoothing"));
        ui.add(egui::Slider::new(&mut self.p.window_ms, 60.0..=1000.0).logarithmic(true).suffix(" ms").text("window"));
        ui.add(egui::Slider::new(&mut self.p.top_db, -90.0..=6.0).suffix(" dB").text("top"));
        ui.add(egui::Slider::new(&mut self.p.range_db, 24.0..=96.0).suffix(" dB").text("range"));
        ui.add(egui::Slider::new(&mut self.p.a4, 415.0..=466.0).text("A4 Hz"));
        ui.add(egui::Slider::new(&mut self.p.units_per_semitone, 0.01..=10.0).logarithmic(true).text("units/semi"));
        ui.horizontal(|ui| {
            ui.label("from");
            if ui.small_button("−").clicked() {
                self.p.base_midi = (self.p.base_midi - 12).max(12);
            }
            ui.label(pitch::note_name(self.p.base_midi));
            if ui.small_button("+").clicked() {
                self.p.base_midi = (self.p.base_midi + 12).min(84);
            }
            if ui.small_button("none").clicked() {
                self.p.enabled.fill(false);
            }
        });
        // One row per octave, one tiny toggle per note.
        ui.spacing_mut().item_spacing = vec2(1.0, 1.0);
        for oct in 0..OCTAVES {
            ui.horizontal(|ui| {
                for n in 0..12 {
                    let i = oct * 12 + n;
                    let name = pitch::NOTE_NAMES[n];
                    let label = egui::RichText::new(name).size(9.0);
                    let b = egui::Button::selectable(self.p.enabled[i], label).min_size(vec2(17.0, 15.0));
                    if ui.add(b).on_hover_text(pitch::note_name(self.p.base_midi + i as i32)).clicked() {
                        self.p.enabled[i] = !self.p.enabled[i];
                    }
                }
            });
        }
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, _shared: &Shared) {
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

        let (lo, hi) = self.p.span();
        let label_h = 13.0;
        let plot = Rect::from_min_max(pos2(rect.left(), rect.top() + 2.0 * label_h + 2.0), pos2(rect.right(), rect.bottom() - label_h));
        let x_of = |m: f32| rect.left() + (m - lo) / (hi - lo) * rect.width();
        let px_per_semi = rect.width() / (hi - lo);
        let small = FontId::monospace(9.0);

        // ---- easing -------------------------------------------------------
        if self.spec.len() != self.spec_target.len() {
            self.spec = self.spec_target.clone();
        }
        let (up, down) = (ease(dt, 0.025), ease(dt, 0.2));
        for (d, &t) in self.spec.iter_mut().zip(&self.spec_target) {
            *d += (t - *d) * if t > *d { up } else { down };
        }
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
                    .filter(|&i| dist(&self.tracks[i]) < ABSORB_SEMIS && self.tracks[i].weight >= SHOW_WEIGHT)
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
            let target = if shown { ((t.clarity - 0.4) / 0.6).clamp(0.0, 1.0).max(0.25) * level } else { 0.0 };
            t.alpha += (target - t.alpha) * if target > t.alpha { ease(dt, 0.12) } else { ease(dt, 0.45) };
            t.primary += (t.target_primary - t.primary) * ease(dt, 0.15);
        }
        self.tracks.retain(|t| t.weight > 0.01 || t.alpha > 0.004);

        let sure = self.midi.is_some() && self.pick_clarity >= SURE;
        if let (true, Some(m)) = (sure, self.midi) {
            self.shown_midi = m;
        }
        let want = if sure { level } else { 0.0 };
        self.readout += (want - self.readout) * if want > self.readout { ease(dt, 0.2) } else { ease(dt, 0.6) };

        // ---- grid ---------------------------------------------------------
        let target = pitch::snap(self.shown_midi, self.enabled_notes());
        for i in 0..NOTES {
            let m = self.p.base_midi + i as i32;
            let x = x_of(m as f32);
            let n = m.rem_euclid(12) as usize;
            let sharp = pitch::NOTE_NAMES[n].len() > 1;
            let on = self.is_enabled(m);
            if on {
                let col = Rect::from_x_y_ranges(x - px_per_semi * 0.5..=x + px_per_semi * 0.5, plot.y_range());
                painter.rect_filled(col, 0.0, theme::ACCENT.gamma_multiply(0.07));
            }
            let line = if on {
                theme::ACCENT_BRIGHT.gamma_multiply(0.35)
            } else if n == 0 {
                Color32::from_gray(58)
            } else if sharp {
                Color32::from_gray(26)
            } else {
                Color32::from_gray(36)
            };
            painter.vline(x, plot.y_range(), Stroke::new(1.0, line));
            if !sharp || px_per_semi > 22.0 {
                let text = if n == 0 { pitch::note_name(m) } else { pitch::NOTE_NAMES[n].to_owned() };
                let c = if on {
                    theme::ACCENT_BRIGHT.gamma_multiply(0.85)
                } else if n == 0 {
                    Color32::from_gray(130)
                } else {
                    Color32::from_gray(if sharp { 62 } else { 88 })
                };
                painter.text(pos2(x, rect.bottom() - 1.0), Align2::CENTER_BOTTOM, text, small.clone(), c);
            }
        }
        let top_db = self.p.top_db;
        let bottom_db = top_db - self.p.range_db;
        let db_scale = Scale::linear((bottom_db, top_db), (plot.bottom(), plot.top()));
        let y_of = |db: f32| db_scale.px(db.clamp(bottom_db, top_db));
        Axis::new(Edge::Left, db_scale).spacing(28.0).minor(false).draw(&painter, plot, |db| format!("{db:.0}"));

        // Target note of the readout, faint, under the spectrum.
        if let Some((n, _)) = target
            && self.readout > 0.01
        {
            painter.vline(x_of(n as f32), plot.y_range(), Stroke::new(2.0, theme::GOOD.gamma_multiply(0.25 * self.readout)));
        }

        // ---- spectrum -----------------------------------------------------
        if self.spec.len() > 1 {
            let count = self.spec.len();
            let pts: Vec<Pos2> = self
                .spec
                .iter()
                .enumerate()
                .map(|(k, &d)| pos2(x_of(lo + (hi - lo) * k as f32 / (count - 1) as f32), y_of(d)))
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
            let base = lerp_color(Color32::from_rgb(196, 176, 150), theme::ACCENT_BRIGHT, t.primary);
            let col = base.gamma_multiply(t.alpha);
            painter.vline(x, plot.y_range(), Stroke::new(1.0 + 0.8 * t.primary, col));
            let text = format!("{} {:.0}%", pitch::note_name(t.midi.round() as i32), t.clarity * 100.0);
            let galley = painter.layout_no_wrap(text, FontId::monospace(10.0), col);
            let w = galley.size().x;
            let left = (x - w / 2.0).clamp(rect.left() + 1.0, rect.right() - w - 1.0);
            let row = lanes.place(left, w);
            painter.galley(pos2(left, rect.top() + 2.0 + row as f32 * label_h), galley, col);
        }

        // ---- readout ------------------------------------------------------
        if self.readout > 0.01
            && let Some((n, cents)) = target
        {
            let a = self.readout;
            let in_tune = cents.abs() < 3.0;
            let col = if in_tune { theme::GOOD } else { theme::ACCENT_BRIGHT }.gamma_multiply(a);
            let dim = Color32::from_gray(170).gamma_multiply(a);
            let units = cents / 100.0 * self.p.units_per_semitone;
            let advice = if in_tune {
                "in tune".to_owned()
            } else if cents > 0.0 {
                format!("▼ down {:.2}", units.abs())
            } else {
                format!("▲ up {:.2}", units.abs())
            };
            let hz = pitch::midi_to_freq(self.shown_midi, self.p.a4);

            let w = 176.0;
            let panel = Rect::from_min_size(pos2(plot.right() - w - 8.0, plot.top() + 6.0), vec2(w, 64.0));
            painter.rect_filled(panel, 2.0, Color32::from_black_alpha((150.0 * a) as u8));
            painter.text(panel.left_top() + vec2(8.0, 4.0), Align2::LEFT_TOP, pitch::note_name(n), FontId::proportional(26.0), col);
            painter.text(panel.right_top() + vec2(-8.0, 6.0), Align2::RIGHT_TOP, &advice, FontId::proportional(14.0), col);
            painter.text(panel.right_top() + vec2(-8.0, 24.0), Align2::RIGHT_TOP, format!("{cents:+.1}¢  {hz:.2} Hz"), FontId::monospace(10.0), dim);
            // ±50 cent strip.
            let strip = Rect::from_min_size(panel.left_bottom() + vec2(8.0, -14.0), vec2(w - 16.0, 6.0));
            painter.hline(strip.x_range(), strip.center().y, Stroke::new(1.0, Color32::from_gray(70).gamma_multiply(a)));
            painter.vline(strip.center().x, strip.y_range(), Stroke::new(1.0, theme::GOOD.gamma_multiply(0.6 * a)));
            let mx = strip.center().x + (cents / 50.0).clamp(-1.0, 1.0) * strip.width() / 2.0;
            painter.circle_filled(pos2(mx, strip.center().y), 3.0, col);
        }
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decimated low E with harmonics still lands on 41.2 Hz.
    #[test]
    fn decimated_low_e() {
        let rate = 48_000;
        let x: Vec<f32> = (0..rate as usize / 3)
            .map(|i| {
                let t = std::f32::consts::TAU * 41.2 * i as f32 / rate as f32;
                0.3 * t.sin() + 0.4 * (2.0 * t).sin() + 0.2 * (3.0 * t).sin()
            })
            .collect();
        let d = Decimator::new(rate, 300.0);
        assert!(d.factor > 1);
        let y = d.run(&x);
        let f = pitch::candidates(&y, d.out_rate(), 30.0, 300.0).pick().unwrap();
        assert!((f - 41.2).abs() < 0.05, "{f}");
    }
}

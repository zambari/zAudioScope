//! What the waterfall and Impulse share: rows of log-band
//! spectra taken at a fixed rate of audio time, their persisted settings
//! (FFT size, history, colour scale, speed, gating), the parameter UI, the
//! colour ramp and the speed handle on the graph's right edge.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use egui::{Color32, Rect, Ui};

use super::Shared;
use crate::audio::spectrum::Bands;
use crate::ui::dynamic_range::DynamicRange;
use crate::ui::handle::EdgeHandle;
use crate::ui::noterange::NoteRange;
use crate::ui::plot::{self, Edge, Link, Scale};
use crate::ui::theme;

/// Across the note bar's whole span (8 per semitone); a module shows the
/// slice in the selected range, so changing the range keeps the history.
pub const BANDS: usize = 640;
const ROW_MS: (f32, f32) = (2.0, 100.0);
/// Most rows added in one frame; further behind than that, skip ahead.
const MAX_ROWS_PER_FRAME: usize = 16;
/// Longest catch-up span, ms, so slow rows don't inflate the per-frame snapshot.
const MAX_CATCHUP_MS: f32 = 400.0;
const ROWS: (usize, usize) = (20, 600);
/// FFT sizes offered as (2^n, label).
const FFT_SIZES: &[(u32, &str)] = &[(12, "low"), (14, "medium"), (16, "ultra")];

#[derive(Clone, Copy, PartialEq)]
pub enum Key {
    Speed,
}

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    pub fft_pow: u32,
    pub rows: usize,
    /// Colour scale: dark at the floor, white at the ceiling.
    pub db: DynamicRange,
    /// Legacy flat floor / span / ceiling, read once and migrated to `db`.
    #[serde(skip_serializing)]
    floor_db: f32,
    #[serde(skip_serializing)]
    ceil_db: f32,
    #[serde(skip_serializing)]
    range_db: f32,
    /// Only analyse the gated window (when the scope has one).
    pub follow_gate: bool,
    /// Audio time per row, so the scroll speed doesn't depend on the frame rate.
    pub row_ms: f32,
    /// Colour ramp for the level.
    pub palette: Palette,
}

/// Colour ramps for the rows' levels, floor first.
#[derive(Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum Palette {
    /// Dark → accent orange → near white (the impulse ramp).
    Mono,
    /// Black → muted red → orange → yellow → white.
    #[default]
    Fire,
    /// Black → indigo → maroon → gold → white.
    Plasma,
}

impl Palette {
    pub const ALL: [Self; 3] = [Self::Fire, Self::Plasma, Self::Mono];

    pub fn name(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Fire => "fire",
            Self::Plasma => "plasma",
        }
    }

    /// Colour for a level in 0..=1.
    pub fn color(self, t: f32) -> Color32 {
        match self {
            Self::Mono => heat(t),
            Self::Fire => ramp(FIRE, t),
            Self::Plasma => ramp(PLASMA, t),
        }
    }
}

const FIRE: &[(f32, Color32)] = &[
    (0.0, Color32::from_rgb(0, 0, 0)),
    (0.25, Color32::from_rgb(88, 26, 18)),
    (0.5, Color32::from_rgb(180, 66, 36)),
    (0.75, Color32::from_rgb(250, 160, 70)),
    (1.0, Color32::from_rgb(255, 250, 225)),
];

const PLASMA: &[(f32, Color32)] = &[
    (0.0, Color32::from_rgb(0, 0, 0)),
    (0.38, Color32::from_rgb(40, 38, 112)),
    (0.61, Color32::from_rgb(78, 24, 24)),
    (0.88, Color32::from_rgb(255, 192, 0)),
    (1.0, Color32::from_rgb(255, 250, 240)),
];

/// Colour at `t` along `stops` (sorted by position), linear between them.
fn ramp(stops: &[(f32, Color32)], t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    for w in stops.windows(2) {
        let ((a, ca), (b, cb)) = (w[0], w[1]);
        if t <= b {
            return theme::lerp(ca, cb, (t - a) / (b - a));
        }
    }
    stops[stops.len() - 1].1
}

impl Default for Params {
    fn default() -> Self {
        Self {
            fft_pow: 12,
            rows: 160,
            db: DynamicRange::default(),
            floor_db: f32::NAN,
            ceil_db: f32::NAN,
            range_db: f32::NAN,
            follow_gate: false,
            row_ms: 15.0,
            palette: Palette::default(),
        }
    }
}

impl Params {
    pub fn with_rows(rows: usize) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// Pull loaded values back into the ranges the UI allows.
    pub fn sanitize(&mut self) {
        // Snap to the nearest FFT size offered (older saves may hold others).
        self.fft_pow = FFT_SIZES
            .iter()
            .map(|&(pow, _)| pow)
            .min_by_key(|pow| pow.abs_diff(self.fft_pow))
            .unwrap_or(FFT_SIZES[0].0);
        self.rows = self.rows.clamp(ROWS.0, ROWS.1);
        if self.range_db.is_finite() {
            self.ceil_db = self.floor_db + self.range_db;
            self.range_db = f32::NAN;
        }
        self.db.migrate(&mut self.floor_db, &mut self.ceil_db);
        self.db.sanitize();
        self.row_ms = self.row_ms.clamp(ROW_MS.0, ROW_MS.1);
    }

    /// Load from `key`, sanitized.
    pub fn load(storage: &dyn eframe::Storage, key: &str) -> Option<Self> {
        let mut p = eframe::get_value::<Self>(storage, key)?;
        p.sanitize();
        Some(p)
    }

    /// Level on the colour scale, 0 at the floor, 1 at the ceiling.
    pub fn level(&self, db: f32) -> f32 {
        self.db.level(db)
    }

    pub fn color(&self, db: f32) -> Color32 {
        self.palette.color(self.level(db))
    }

    /// Seconds of history shown.
    pub fn secs(&self) -> f32 {
        self.rows as f32 * self.row_ms / 1000.0
    }

    /// Samples of recent audio needed per frame: one FFT plus the rows it
    /// may still have to catch up on.
    pub fn lookback(&self, rate: u32) -> usize {
        (1usize << self.fft_pow) + (self.catchup_ms() * rate as f32 / 1000.0) as usize
    }

    /// How far behind "now" rows may still be caught up; further, skip ahead.
    fn catchup_ms(&self) -> f32 {
        (self.row_ms * MAX_ROWS_PER_FRAME as f32).min(MAX_CATCHUP_MS.max(self.row_ms))
    }

    pub fn ui(&mut self, ui: &mut Ui, shared: &Shared, link: &mut Link<Key>) {
        self.db.ui(ui, shared);
        self.ui_fft(ui);
        ui.add(egui::Slider::new(&mut self.rows, ROWS.0..=ROWS.1).text("history rows"))
            .on_hover_text(
                "Number of spectrum rows retained. Time between rows is set by ms / row.",
            );
        link.slider(
            ui,
            Key::Speed,
            egui::Slider::new(&mut self.row_ms, ROW_MS.0..=ROW_MS.1)
                .logarithmic(true)
                .text("ms / row"),
        );
        self.ui_gate(ui);
    }

    pub fn ui_fft(&mut self, ui: &mut Ui) {
        let name = |pow: u32| FFT_SIZES.iter().find(|s| s.0 == pow).map_or("", |s| s.1);
        egui::ComboBox::from_label("FFT")
            .selected_text(name(self.fft_pow))
            .show_ui(ui, |ui| {
                for &(pow, label) in FFT_SIZES {
                    ui.selectable_value(&mut self.fft_pow, pow, label)
                        .on_hover_text(format!("{} samples", 1u32 << pow));
                }
            });
    }

    pub fn ui_gate(&mut self, ui: &mut Ui) {
        ui.checkbox(&mut self.follow_gate, "only gated window");
    }

    /// Fall-time handle on the right edge (up is shorter), just inside the
    /// dB rail. Call [`Link::tick`] earlier in the frame.
    pub fn fall_handle(&mut self, ui: &mut Ui, rect: Rect, id: &str, link: &mut Link<Key>) {
        let rows = self.rows as f32;
        let fmt = |ms: f32| {
            format!(
                "fall {} · {} ms/row",
                plot::fmt_secs(rows * ms / 1000.0),
                plot::trim(ms)
            )
        };
        time_handle(
            ui,
            rect,
            TimeHandle {
                id,
                edge: Edge::Right,
                range: ROW_MS,
                reset: 15.0,
                fmt: &fmt,
            },
            link,
            &mut self.row_ms,
        );
    }
}

/// Settings of a log-scaled [`time_handle`] on one graph edge.
pub struct TimeHandle<'a> {
    pub id: &'a str,
    pub edge: Edge,
    /// Value range; the top of the edge is the lower end (up is shorter).
    pub range: (f32, f32),
    pub reset: f32,
    pub fmt: &'a dyn Fn(f32) -> String,
}

/// Log-scaled time handle on a graph edge, linked to the [`Key::Speed`]
/// slider (the slider is outlined while the handle is in use; no guide line
/// is drawn, a time has no level on the graph). Call [`Link::tick`] earlier
/// in the frame.
pub fn time_handle(ui: &mut Ui, rect: Rect, h: TimeHandle, link: &mut Link<Key>, value: &mut f32) {
    let scale = Scale::log(h.range, (rect.top() + 6.0, rect.bottom() - 6.0));
    let r = EdgeHandle::new(h.id, h.edge, scale, value)
        .reset(h.reset)
        .guide(false)
        .label(h.fmt)
        .show(ui, rect);
    if r.hovered() || r.dragged() {
        link.touch(Key::Speed);
    }
}

/// Dark → accent orange → near white.
pub fn heat(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let base = Color32::from_gray(13);
    if t < 0.6 {
        theme::lerp(base, theme::ACCENT, t / 0.6)
    } else {
        theme::lerp(theme::ACCENT, theme::ON_ACCENT, (t - 0.6) / 0.4)
    }
}

/// Band index range `[lo, hi)` and the bounds-relative span `(u0, u1)` of
/// the shared note range's selection.
pub fn visible_bands(range: NoteRange) -> ((usize, usize), (f32, f32)) {
    let (a, b) = range.span();
    let (u0, u1) = (range.bounds_pos(a), range.bounds_pos(b));
    let lo = (u0 * BANDS as f32).floor().max(0.0) as usize;
    let hi = ((u1 * BANDS as f32).ceil() as usize).min(BANDS);
    ((lo, hi), (u0, u1))
}

/// Rolling history of band spectra in dB, newest first.
#[derive(Default)]
pub struct Rows {
    /// Band layout for the current FFT size and rate.
    bands: Option<Bands>,
    pub history: VecDeque<Vec<f32>>,
    /// Absolute sample position the next row ends at.
    next_at: Option<u64>,
}

/// What [`Rows::update`] did.
#[derive(Default)]
pub struct Added {
    /// History was dropped (new FFT size or rate).
    pub cleared: bool,
    /// Rows added this frame (newest at the front of `history`).
    pub rows: usize,
}

impl Rows {
    pub fn update(&mut self, p: &Params, shared: &Shared) -> Added {
        let mut out = Added::default();
        let n = 1usize << p.fft_pow;
        let f = &shared.frame;
        let edges = shared.range.get().bounds().hz();
        if self
            .bands
            .as_ref()
            .is_none_or(|b| !b.fits(n, f.rate, edges))
        {
            self.bands = Some(Bands::new(n, f.rate, BANDS, edges));
            self.history.clear();
            out.cleared = true;
        }
        let hop = f.ms_to_samples(p.row_ms).max(32) as u64;
        let end = f.end();
        let catchup = f.ms_to_samples(p.catchup_ms()) as u64;
        // Start at "now" on the first frame, after a restart, or when too far
        // behind to catch up: missed rows are skipped, not burst out.
        let mut t = match self.next_at {
            Some(t) if !shared.reset && t <= end + hop && end - t.min(end) <= catchup => t,
            _ => end,
        };
        let Some(bands) = self.bands.as_ref() else {
            return out;
        };
        while t <= end {
            // Gated: the window itself (shared with anyone else asking for
            // it this frame). Otherwise the `n` samples ending at `t`.
            let range = match shared.gate.window {
                Some(w) if p.follow_gate => w,
                _ => (t.saturating_sub(n as u64), t),
            };
            self.history
                .push_front(bands.db(&shared.spectrum(n, range)));
            out.rows += 1;
            t += hop;
        }
        self.history.truncate(p.rows);
        self.next_at = Some(t);
        out
    }
}

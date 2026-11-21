//! Rolling spectrum waterfall, newest row at the top. Uses the warped
//! frequency axis from KitPool's sample editor so the bass gets room.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use egui::{Color32, ColorImage, Rect, TextureHandle, TextureOptions, Ui};

use super::{Module, Shared};
use crate::audio::spectrum::{self, Bands};
use crate::ui::theme;

const BANDS: usize = 256;

/// The persisted settings.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Params {
    fft_pow: u32,
    rows: usize,
    floor_db: f32,
    range_db: f32,
    /// Only analyse the gated window (when the scope has one).
    follow_gate: bool,
    /// Audio time per row, so the scroll speed doesn't depend on the frame rate.
    row_ms: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self { fft_pow: 12, rows: 160, floor_db: -90.0, range_db: 80.0, follow_gate: false, row_ms: 15.0 }
    }
}

/// Most rows added in one frame; further behind than that, skip ahead.
const MAX_ROWS_PER_FRAME: usize = 16;

impl Params {
    /// Pull loaded values back into the ranges the UI allows.
    fn sanitize(&mut self) {
        self.fft_pow = self.fft_pow.clamp(10, 15);
        self.rows = self.rows.clamp(20, 600);
        self.floor_db = self.floor_db.clamp(spectrum::FLOOR_DB, -20.0);
        self.range_db = self.range_db.clamp(20.0, 100.0);
        self.row_ms = self.row_ms.clamp(2.0, 100.0);
    }
}

#[derive(Default)]
pub struct Waterfall {
    p: Params,
    /// Band layout for the current FFT size and rate.
    bands: Option<Bands>,
    /// Rows in dB, newest first.
    history: VecDeque<Vec<f32>>,
    /// The same rows as colours, made once per row instead of every frame.
    colored: VecDeque<Vec<Color32>>,
    /// (floor, range, rows) the colours and texture were made with.
    colored_for: (f32, f32, usize),
    /// Absolute sample position the next row ends at.
    next_at: Option<u64>,
    /// The texture is out of date.
    dirty: bool,
    texture: Option<TextureHandle>,
}

/// Dark → accent orange → near white.
fn heat(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: Color32, b: Color32, u: f32| {
        let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * u) as u8;
        Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
    };
    let base = Color32::from_gray(13);
    if t < 0.6 {
        lerp(base, theme::ACCENT, t / 0.6)
    } else {
        lerp(theme::ACCENT, theme::ON_ACCENT, (t - 0.6) / 0.4)
    }
}

impl Params {
    fn color(&self, db: f32) -> Color32 {
        heat((db - self.floor_db) / self.range_db)
    }
}

impl Module for Waterfall {
    fn title(&self) -> &'static str {
        "Waterfall"
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
        200.0
    }

    fn lookback(&self, rate: u32) -> usize {
        // One FFT plus the rows it may still need to catch up on.
        (1usize << self.p.fft_pow) + (self.p.row_ms * MAX_ROWS_PER_FRAME as f32 * rate as f32 / 1000.0) as usize
    }

    fn update(&mut self, shared: &mut Shared) {
        let n = 1usize << self.p.fft_pow;
        let f = &shared.frame;
        if self.bands.as_ref().is_none_or(|b| !b.fits(n, f.rate)) {
            self.bands = Some(Bands::new(n, f.rate, BANDS));
            self.history.clear();
            self.colored.clear();
            self.dirty = true;
        }
        let hop = f.ms_to_samples(self.p.row_ms).max(32) as u64;
        let end = f.end();
        // Start at "now" on the first frame, after a restart, or when too far
        // behind to catch up: missed rows are skipped, not burst out.
        let next = match self.next_at {
            Some(t) if !shared.reset && t <= end + hop && end - t.min(end) <= hop * MAX_ROWS_PER_FRAME as u64 => t,
            _ => end,
        };
        let mut t = next;
        let Some(bands) = self.bands.as_ref() else { return };
        while t <= end {
            // Gated: the window itself (shared with anyone else asking for
            // it this frame). Otherwise the `n` samples ending at `t`.
            let range = match shared.gate.window {
                Some(w) if self.p.follow_gate => w,
                _ => (t.saturating_sub(n as u64), t),
            };
            let row = bands.db(&shared.spectrum(n, range));
            self.colored.push_front(row.iter().map(|&db| self.p.color(db)).collect());
            self.history.push_front(row);
            t += hop;
        }
        if t != next {
            self.history.truncate(self.p.rows);
            self.colored.truncate(self.p.rows);
            self.dirty = true;
        }
        self.next_at = Some(t);
    }

    fn params(&mut self, ui: &mut Ui) {
        ui.add(egui::Slider::new(&mut self.p.fft_pow, 10..=15).text("FFT 2^n"));
        ui.add(egui::Slider::new(&mut self.p.rows, 20..=600).text("history"));
        ui.add(egui::Slider::new(&mut self.p.floor_db, spectrum::FLOOR_DB..=-20.0).text("floor dB"));
        ui.add(egui::Slider::new(&mut self.p.range_db, 20.0..=100.0).text("range dB"));
        ui.add(egui::Slider::new(&mut self.p.row_ms, 2.0..=100.0).logarithmic(true).text("ms / row"));
        ui.checkbox(&mut self.p.follow_gate, "only gated window");
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        if self.history.is_empty() {
            return;
        }
        // Colour scale changed: recolour what we have.
        let key = (self.p.floor_db, self.p.range_db, self.p.rows);
        if self.colored_for != key {
            self.colored_for = key;
            self.colored = self.history.iter().map(|r| r.iter().map(|&db| self.p.color(db)).collect()).collect();
            self.dirty = true;
        }
        if self.texture.is_some() && !self.dirty {
            self.paint(ui, rect, shared);
            return;
        }
        self.dirty = false;
        let mut img = ColorImage::filled([BANDS, self.p.rows], Color32::from_gray(13));
        for (y, row) in self.colored.iter().take(self.p.rows).enumerate() {
            img.pixels[y * BANDS..y * BANDS + row.len().min(BANDS)].copy_from_slice(&row[..row.len().min(BANDS)]);
        }
        match &mut self.texture {
            Some(t) => t.set(img, TextureOptions::LINEAR),
            None => {
                self.texture = Some(ui.ctx().load_texture("waterfall", img, TextureOptions::LINEAR));
            }
        }
        self.paint(ui, rect, shared);
    }
}

impl Waterfall {
    fn paint(&self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        let Some(tex) = &self.texture else { return };
        ui.painter_at(rect).image(
            tex.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        // Frequency grid.
        let f_max = shared.frame.rate as f32 / 2.0;
        for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
            if f >= f_max {
                break;
            }
            let x = rect.left() + spectrum::axis_pos(f, f_max) * rect.width();
            ui.painter().vline(x, rect.y_range(), egui::Stroke::new(1.0, Color32::from_white_alpha(18)));
            let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
            ui.painter().text(
                egui::pos2(x + 2.0, rect.bottom() - 2.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                egui::FontId::monospace(10.0),
                Color32::from_white_alpha(90),
            );
        }
    }
}

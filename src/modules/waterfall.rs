//! Rolling spectrum waterfall, newest row at the top. Uses the warped
//! frequency axis from KitPool's sample editor so the bass gets room.
//! Handles: two along the bottom pick the shown frequency range (placed on
//! the full-range axis, with a thin strip marking the zoom), one on the
//! right sets the scroll speed.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use egui::{Color32, ColorImage, Rect, Stroke, TextureHandle, TextureOptions, Ui, pos2};

use super::{Module, Shared};
use crate::audio::spectrum::{self, Bands};
use crate::ui::handle::EdgeHandle;
use crate::ui::plot::{self, Axis, Edge, Link, Scale};
use crate::ui::theme;

/// Across the full range; zooming shows a slice of these.
const BANDS: usize = 512;
const ROW_MS: (f32, f32) = (2.0, 100.0);
/// Narrowest zoom, as a ratio of top to bottom frequency.
const MIN_ZOOM: f32 = 1.25;

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Lo,
    Hi,
    Speed,
}

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
    /// Shown frequency range, Hz.
    f_lo: f32,
    f_hi: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            fft_pow: 12,
            rows: 160,
            floor_db: -90.0,
            range_db: 80.0,
            follow_gate: false,
            row_ms: 15.0,
            f_lo: spectrum::F_MIN,
            f_hi: 24_000.0,
        }
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
        self.row_ms = self.row_ms.clamp(ROW_MS.0, ROW_MS.1);
        self.f_hi = self.f_hi.clamp(spectrum::F_MIN * MIN_ZOOM, 48_000.0);
        self.f_lo = self.f_lo.clamp(spectrum::F_MIN, self.f_hi / MIN_ZOOM);
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
    link: Link<Key>,
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
        let link = &mut self.link;
        link.slider(ui, Key::Speed, egui::Slider::new(&mut self.p.row_ms, ROW_MS.0..=ROW_MS.1).logarithmic(true).text("ms / row"));
        link.slider(ui, Key::Lo, egui::Slider::new(&mut self.p.f_lo, spectrum::F_MIN..=5000.0).logarithmic(true).suffix(" Hz").text("from"));
        link.slider(ui, Key::Hi, egui::Slider::new(&mut self.p.f_hi, 50.0..=24_000.0).logarithmic(true).suffix(" Hz").text("to"));
        self.p.f_lo = self.p.f_lo.min(self.p.f_hi / MIN_ZOOM);
        ui.checkbox(&mut self.p.follow_gate, "only gated window");
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
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
    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        let Some(tex) = self.texture.as_ref().map(|t| t.id()) else { return };
        let painter = ui.painter_at(rect);
        let f_max = shared.frame.rate as f32 / 2.0;
        let f_hi = self.p.f_hi.min(f_max);
        let f_lo = self.p.f_lo.min(f_hi / MIN_ZOOM);
        let (u0, u1) = (spectrum::axis_pos(f_lo, f_max), spectrum::axis_pos(f_hi, f_max));
        painter.image(tex, rect, Rect::from_min_max(pos2(u0, 0.0), pos2(u1, 1.0)), Color32::WHITE);

        // Frequency along the bottom (zoomed), time ago down the left.
        let shown = Scale::warped(spectrum::warp, spectrum::unwarp, (f_lo, f_hi), (rect.left(), rect.right()));
        Axis::new(Edge::Bottom, shown).spacing(48.0).draw(&painter, rect, plot::fmt_hz);
        let secs = self.p.rows as f32 * self.p.row_ms / 1000.0;
        Axis::new(Edge::Left, Scale::linear((0.0, secs), (rect.top(), rect.bottom())))
            .spacing(32.0)
            .grid(false)
            .draw(&painter, rect, plot::fmt_secs);

        // Zoom handles live on the full-range axis; a strip shows the slice.
        let full = Scale::warped(spectrum::warp, spectrum::unwarp, (spectrum::F_MIN, f_max), (rect.left(), rect.right()));
        if f_lo > spectrum::F_MIN * 1.01 || f_hi < f_max * 0.99 {
            let y = rect.bottom() - 1.0;
            painter.hline(rect.x_range(), y, Stroke::new(2.0, Color32::from_white_alpha(14)));
            painter.hline(full.px(f_lo)..=full.px(f_hi), y, Stroke::new(2.0, theme::HANDLE.gamma_multiply(0.45)));
        }
        let fmt = |f: f32| format!("from {} Hz", plot::fmt_hz(f));
        let r = EdgeHandle::new("wf_lo", Edge::Bottom, full, &mut self.p.f_lo)
            .clamp(spectrum::F_MIN, f_hi / MIN_ZOOM)
            .reset(spectrum::F_MIN)
            .guide(false)
            .label(&fmt)
            .show(ui, rect);
        if r.hovered() || r.dragged() {
            self.link.touch(Key::Lo);
        }
        let fmt = |f: f32| format!("to {} Hz", plot::fmt_hz(f));
        let r = EdgeHandle::new("wf_hi", Edge::Bottom, full, &mut self.p.f_hi)
            .clamp(self.p.f_lo * MIN_ZOOM, f_max)
            .reset(f_max)
            .guide(false)
            .label(&fmt)
            .show(ui, rect);
        if r.hovered() || r.dragged() {
            self.link.touch(Key::Hi);
        }

        // Speed on the right: up is faster.
        let speed = Scale::log(ROW_MS, (rect.top() + 6.0, rect.bottom() - 6.0));
        let rows = self.p.rows as f32;
        let fmt = |ms: f32| format!("{} ms/row · {}", plot::trim(ms), plot::fmt_secs(rows * ms / 1000.0));
        let r = EdgeHandle::new("wf_speed", Edge::Right, speed, &mut self.p.row_ms).reset(15.0).guide(false).label(&fmt).show(ui, rect);
        if r.hovered() || r.dragged() {
            self.link.touch(Key::Speed);
        }

        match self.link.shown() {
            Some(Key::Lo) => self.link.point_x(ui, rect, full.px(self.p.f_lo)),
            Some(Key::Hi) => self.link.point_x(ui, rect, full.px(self.p.f_hi.min(f_max))),
            Some(Key::Speed) => self.link.point_y(ui, rect, speed.px(self.p.row_ms)),
            None => {}
        }
    }
}

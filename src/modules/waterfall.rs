//! Rolling spectrum waterfall, newest row at the top. Uses the warped
//! frequency axis from KitPool's sample editor so the bass gets room.

use std::collections::VecDeque;

use egui::{Color32, ColorImage, Rect, TextureHandle, TextureOptions, Ui};

use super::{Module, Shared};
use crate::audio::spectrum::{self, Analyzer};
use crate::ui::theme;

const BANDS: usize = 256;

pub struct Waterfall {
    fft_pow: u32,
    rows: usize,
    floor_db: f32,
    range_db: f32,
    /// Only analyse the gated window (when the scope has one).
    follow_gate: bool,
    analyzer: Option<Analyzer>,
    history: VecDeque<Vec<f32>>,
    texture: Option<TextureHandle>,
}

impl Default for Waterfall {
    fn default() -> Self {
        Self {
            fft_pow: 12,
            rows: 160,
            floor_db: -90.0,
            range_db: 80.0,
            follow_gate: false,
            analyzer: None,
            history: VecDeque::new(),
            texture: None,
        }
    }
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

impl Module for Waterfall {
    fn title(&self) -> &'static str {
        "Waterfall"
    }

    fn graph_height(&self) -> f32 {
        200.0
    }

    fn update(&mut self, shared: &mut Shared) {
        let n = 1usize << self.fft_pow;
        let rate = shared.frame.rate;
        if self.analyzer.as_ref().is_none_or(|a| a.size() != n || a.rate() != rate) {
            self.analyzer = Some(Analyzer::new(n, rate, BANDS));
            self.history.clear();
        }
        let x = if self.follow_gate { shared.analysis_slice(n) } else { shared.frame.samples.as_slice() };
        let x = &x[x.len().saturating_sub(n)..];
        let Some(an) = self.analyzer.as_mut() else { return };
        let row = an.frame(|i| x[i], x.len(), 0);
        self.history.push_front(row);
        self.history.truncate(self.rows);
    }

    fn params(&mut self, ui: &mut Ui) {
        ui.add(egui::Slider::new(&mut self.fft_pow, 10..=15).text("FFT 2^n"));
        ui.add(egui::Slider::new(&mut self.rows, 20..=600).text("history"));
        ui.add(egui::Slider::new(&mut self.floor_db, spectrum::FLOOR_DB..=-20.0).text("floor dB"));
        ui.add(egui::Slider::new(&mut self.range_db, 20.0..=100.0).text("range dB"));
        ui.checkbox(&mut self.follow_gate, "only gated window");
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        if self.history.is_empty() {
            return;
        }
        let mut img = ColorImage::filled([BANDS, self.rows], Color32::from_gray(13));
        for (y, row) in self.history.iter().enumerate() {
            for (x, db) in row.iter().enumerate() {
                img.pixels[y * BANDS + x] = heat((db - self.floor_db) / self.range_db);
            }
        }
        let tex = match &mut self.texture {
            Some(t) => {
                t.set(img, TextureOptions::LINEAR);
                t
            }
            None => self.texture.insert(ui.ctx().load_texture("waterfall", img, TextureOptions::LINEAR)),
        };
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

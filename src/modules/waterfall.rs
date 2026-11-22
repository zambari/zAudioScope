//! Isometric waterfall (after sample_manager's stats panel): one spectrum
//! per row, newest at the front, older rows stepping back up and to the
//! right and fading into the background. Frequency runs along the shared
//! note range like the spectrogram; height and colour are level. Shares
//! its rows and settings with the spectrogram (see `spectrum_rows`).

use egui::{Align2, Mesh, Rect, Shape, Stroke, Ui, pos2, vec2};

use super::spectrum_rows::{BANDS, Key, Params, Rows, heat, visible_bands};
use super::{Module, Shared};
use crate::ui::plot::{self, Axis, Edge, Link};
use crate::ui::theme;

/// Share of the graph the rows step back by, across and up.
const DEPTH_X: f32 = 0.28;
const DEPTH_Y: f32 = 0.5;
/// Room for frequency labels under the front row.
const LABEL_H: f32 = 16.0;
/// Least pixels between points along a row; denser bands are merged (peak).
const POINT_PX: f32 = 2.0;

pub struct Waterfall {
    p: Params,
    rows: Rows,
    link: Link<Key>,
}

impl Default for Waterfall {
    fn default() -> Self {
        Self { p: Params::with_rows(48), rows: Rows::default(), link: Link::default() }
    }
}

impl Module for Waterfall {
    fn title(&self) -> &'static str {
        "Waterfall"
    }

    // Own key: "Waterfall" used to hold the spectrogram's settings.
    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str) {
        eframe::set_value(storage, &format!("{key}.3d"), &self.p);
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        if let Some(p) = Params::load(storage, &format!("{key}.3d"), None) {
            self.p = p;
        }
    }

    fn graph_height(&self) -> f32 {
        240.0
    }

    fn lookback(&self, rate: u32) -> usize {
        self.p.lookback(rate)
    }

    fn update(&mut self, shared: &mut Shared) {
        self.rows.update(&self.p, shared);
    }

    fn params(&mut self, ui: &mut Ui, _shared: &Shared) {
        self.p.ui(ui, &mut self.link);
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        let bg = ui.visuals().extreme_bg_color;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);

        let plot = Rect::from_min_max(rect.min + vec2(4.0, 4.0), pos2(rect.right() - 4.0, rect.bottom() - LABEL_H));
        let depth = vec2(plot.width() * DEPTH_X, -plot.height() * DEPTH_Y);
        let amp_h = plot.height() * (1.0 - DEPTH_Y);
        let plane_w = plot.width() - depth.x;

        let range = shared.range.get();
        let ((lo, hi), (u0, u1)) = visible_bands(range);
        let x_of = |j: usize| ((j as f32 + 0.5) / BANDS as f32 - u0) / (u1 - u0) * plane_w;
        let step = (((hi - lo) as f32 * POINT_PX / plane_w).ceil() as usize).max(1);

        let total = self.p.rows.max(2);
        // Back to front, so nearer rows cover the ones behind.
        for (i, row) in self.rows.history.iter().enumerate().rev() {
            let back = i as f32 / (total - 1) as f32;
            let origin = plot.left_bottom() + depth * back;
            let fade = (1.0 - back).powf(2.5);
            let mut mesh = Mesh::default();
            let mut line = Vec::with_capacity((hi - lo) / step + 1);
            for j in (lo..hi).step_by(step) {
                let db = row[j..(j + step).min(hi)].iter().copied().fold(f32::MIN, f32::max);
                let lv = self.p.level(db);
                let x = origin.x + x_of(j + step / 2);
                let top = pos2(x, origin.y - lv * amp_h);
                let v = mesh.vertices.len() as u32;
                mesh.colored_vertex(pos2(x, origin.y), bg);
                mesh.colored_vertex(top, theme::lerp(bg, heat(lv), fade));
                if v > 0 {
                    mesh.add_triangle(v - 2, v - 1, v);
                    mesh.add_triangle(v - 1, v, v + 1);
                }
                line.push(top);
            }
            painter.add(mesh);
            painter.add(Shape::line(line, Stroke::new(1.0, theme::lerp(bg, theme::WAVE, 0.06 + 0.7 * fade))));
        }

        // Frequency along the front row.
        let front = Rect::from_min_max(pos2(plot.left(), rect.top()), pos2(plot.left() + plane_w, rect.bottom()));
        Axis::new(Edge::Bottom, range.hz_scale(front)).spacing(48.0).grid(false).draw(&painter, front, plot::fmt_hz);
        plot::caption(&painter, rect, Align2::RIGHT_TOP, format!("{} deep", plot::fmt_secs(self.p.secs())));

        // Pointer: frequency under the cursor, read on the front row.
        if let Some(pos) = ui.ctx().pointer_hover_pos().filter(|p| front.contains(*p)) {
            let f = range.hz_scale(front).value(pos.x);
            painter.vline(pos.x, (plot.bottom() - amp_h)..=plot.bottom(), Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.6)));
            plot::caption(&painter, rect, Align2::LEFT_TOP, plot::fmt_hz(f));
        }

        self.p.speed_handle(ui, rect, "waterfall_speed", &mut self.link);
    }
}

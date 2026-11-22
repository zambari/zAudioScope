//! Impulse: a rotatable 3D waterfall (after sample_manager's stats panel): one spectrum
//! per row, newest at the front, older rows stepping back up and to the
//! right and fading into the background. Frequency has its own range;
//! the shared note range highlights the active spectrum. Shares
//! its rows and settings with the waterfall (see `spectrum_rows`).

use egui::{Align2, Mesh, Rect, Stroke, Ui, pos2, vec2};

use super::spectrum_rows::{self, BANDS, Key, Params, Rows, heat};
use super::{Module, Shared};
use crate::ui::noterange::NoteRange;
use crate::ui::plot::{self, Link, Scale};
use crate::ui::theme;
use crate::ui::widgets::RangeSlider;

/// Mouse rotation around the default camera, in degrees.
const ROTATION_LIMIT: f32 = 60.0;
const DEFAULT_ROTATION: (f32, f32) = (-31.0, 0.0);
const DEFAULT_ZOOM: f32 = 1.2;
const ZOOM_LIMITS: (f32, f32) = (0.25, 8.0);
/// Room for frequency labels under the front row.
const LABEL_H: f32 = 16.0;
/// Least pixels between points along a row; denser bands are merged (peak).
const POINT_PX: f32 = 2.0;
/// Time span covered front to back, ms, and the number of slices across it.
const WINDOW_MS: (f32, f32) = (50.0, 4000.0);
const DEFAULT_WINDOW_MS: f32 = 500.0;
const SLICES: (usize, usize) = (20, 100);

pub struct Impulse {
    p: Params,
    range_hz: (f32, f32),
    // Camera framing is session-only; only analysis parameters are persisted.
    rotation: (f32, f32),
    zoom: f32,
    fade_speed: f32,
    /// Time from the front slice to the back one; rows and ms/row follow.
    window_ms: f32,
    slices: usize,
    /// Pan as fractions of the viewport, so resizing preserves the framing.
    pan: (f32, f32),
    rows: Rows,
    link: Link<Key>,
}

impl Default for Impulse {
    fn default() -> Self {
        Self {
            p: Params::with_rows(48),
            window_ms: DEFAULT_WINDOW_MS,
            slices: 48,
            range_hz: (40.0, NoteRange::BOUNDS.hz().1),
            rotation: DEFAULT_ROTATION,
            zoom: DEFAULT_ZOOM,
            fade_speed: 2.5,
            pan: (0.0, 0.0),
            rows: Rows::default(),
            link: Link::default(),
        }
    }
}

impl Module for Impulse {
    fn title(&self) -> &'static str {
        "Impulse"
    }

    fn solo_only(&self) -> bool {
        true
    }

    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str) {
        eframe::set_value(storage, &format!("{key}.3d"), &self.p);
        eframe::set_value(storage, &format!("{key}.range_hz"), &self.range_hz);
        eframe::set_value(storage, &format!("{key}.fade_speed"), &self.fade_speed);
        eframe::set_value(storage, &format!("{key}.window"), &(self.window_ms, self.slices));
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        if let Some(speed) = eframe::get_value::<f32>(storage, &format!("{key}.fade_speed")) {
            if speed.is_finite() {
                self.fade_speed = speed.clamp(0.0, 10.0);
            }
        }
        if let Some(p) = Params::load(storage, &format!("{key}.3d")) {
            self.p = p;
        }
        // Older saves only had rows and ms/row: keep the same depth.
        let (window, slices) = eframe::get_value::<(f32, usize)>(storage, &format!("{key}.window"))
            .unwrap_or(((self.p.rows.max(2) - 1) as f32 * self.p.row_ms, self.p.rows));
        if window.is_finite() {
            self.window_ms = window.clamp(WINDOW_MS.0, WINDOW_MS.1);
        }
        self.slices = slices.clamp(SLICES.0, SLICES.1);
        self.apply_window();
        if let Some((lo, hi)) = eframe::get_value::<(f32, f32)>(storage, &format!("{key}.range_hz"))
        {
            if lo.is_finite() && hi.is_finite() {
                let hi = hi.clamp(41.0, NoteRange::BOUNDS.hz().1);
                self.range_hz = (lo.clamp(40.0, hi - 1.0), hi);
            }
        }

    }

    fn graph_height(&self) -> f32 {
        240.0
    }

    fn lookback(&self, rate: u32) -> usize {
        self.p.lookback(rate)
    }

    fn update(&mut self, shared: &mut Shared) {
        self.p.db.sync(&shared.db);
        self.apply_window();
        self.rows.update(&self.p, shared);
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        self.p.db.ui(ui, shared);
        ui.add(
            RangeSlider::new(
                &mut self.range_hz.0,
                &mut self.range_hz.1,
                40.0..=NoteRange::BOUNDS.hz().1,
            )
            .min_gap(1.0)
            .decimals(0)
            .text("range Hz"),
        );
        self.p.ui_fft(ui);
        self.link.slider(
            ui,
            Key::Speed,
            egui::Slider::new(&mut self.window_ms, WINDOW_MS.0..=WINDOW_MS.1)
                .logarithmic(true)
                .integer()
                .text("window ms"),
        )
        .on_hover_text("Time span from the front slice to the back one.");
        ui.add(egui::Slider::new(&mut self.slices, SLICES.0..=SLICES.1).text("slices"))
            .on_hover_text("Number of spectra spread evenly across the window.");
        self.p.ui_gate(ui);
        ui.add(egui::Slider::new(&mut self.fade_speed, 0.0..=10.0).text("fade speed"))
            .on_hover_text("Higher values fade older rows sooner along the history axis. Zero disables fading.");
        ui.weak("Drag: orbit · middle-drag: pan · scroll: zoom");
        ui.weak("Double-click to reset view");
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        let bg = ui.visuals().extreme_bg_color;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, bg);

        let plot = Rect::from_min_max(
            rect.min + vec2(4.0, 4.0),
            pos2(rect.right() - 4.0, rect.bottom() - LABEL_H),
        );
        // Leave the right edge available for the shared speed handle.
        let drag_rect = Rect::from_min_max(plot.min, plot.max - vec2(16.0, 0.0));
        let response = ui.interact(
            drag_rect,
            ui.scope_id().with("impulse_rotation"),
            egui::Sense::click_and_drag(),
        );
        if response.double_clicked() {
            self.rotation = DEFAULT_ROTATION;
            self.zoom = DEFAULT_ZOOM;
            self.pan = (0.0, 0.0);
        } else if response.dragged_by(egui::PointerButton::Middle) {
            let delta = ui.input(|i| i.pointer.delta());
            self.pan.0 += delta.x / plot.width().max(1.0);
            self.pan.1 += delta.y / plot.height().max(1.0);
        } else if response.dragged_by(egui::PointerButton::Primary) {
            let delta = ui.input(|i| i.pointer.delta());
            self.rotation.0 =
                (self.rotation.0 + delta.x * 0.35).clamp(-ROTATION_LIMIT, ROTATION_LIMIT);
            self.rotation.1 =
                (self.rotation.1 + delta.y * 0.35).clamp(-ROTATION_LIMIT, ROTATION_LIMIT);
        }
        // Consume wheel input here so zooming does not also scroll the module list.
        // Keep the world point under the cursor fixed while changing magnification.
        if response.hovered() {
            let scroll = ui.input_mut(|i| {
                let scroll = i.smooth_scroll_delta.y;
                i.smooth_scroll_delta = egui::Vec2::ZERO;
                scroll
            });
            let zoom = (self.zoom * (scroll * 0.004).exp()).clamp(ZOOM_LIMITS.0, ZOOM_LIMITS.1);
            if let Some(pointer) = response.hover_pos() {
                let anchor = pointer - plot.center();
                let pan = vec2(self.pan.0 * plot.width(), self.pan.1 * plot.height());
                let pan = anchor - (anchor - pan) * (zoom / self.zoom);
                self.pan = (
                    pan.x / plot.width().max(1.0),
                    pan.y / plot.height().max(1.0),
                );
            }
            self.zoom = zoom;
        }
        response.on_hover_cursor(egui::CursorIcon::Grab);

        // Orthographic camera: frequency X, amplitude Y, history Z.
        let (sy, cy) = self.rotation.0.to_radians().sin_cos();
        let (sp, cp) = (25.0 + self.rotation.1).to_radians().sin_cos();
        let rotate = |x: f32, y: f32, z: f32| {
            let (x, z) = (cy * x + sy * z, -sy * x + cy * z);
            (x, cp * y - sp * z, sp * y + cp * z)
        };
        // Fit the whole volume, including its tallest possible peak, at every angle.
        let mut projected = Rect::NOTHING;
        for x in [-1.0, 1.0] {
            for y in [0.0, 0.65] {
                for z in [-0.7, 0.7] {
                    let (x, y, _) = rotate(x, y, z);
                    projected.extend_with(pos2(x, -y));
                }
            }
        }
        let scale = ((plot.width() - 32.0).max(1.0) / projected.width())
            .min((plot.height() - 24.0).max(1.0) / projected.height())
            * self.zoom;
        let project = |x: f32, y: f32, z: f32| {
            let (x, y, _) = rotate(x, y, z);
            plot.center()
                + vec2(self.pan.0 * plot.width(), self.pan.1 * plot.height())
                + (pos2(x, -y) - projected.center()) * scale
        };
        let plane_w = (2.0 * scale).max(1.0);

        let bounds = NoteRange::BOUNDS.hz();
        let band_pos = |hz: f32| (hz / bounds.0).ln() / (bounds.1 / bounds.0).ln();
        let (u0, u1) = (band_pos(self.range_hz.0), band_pos(self.range_hz.1));
        // Only include band centres inside our range (and never below 40 Hz).
        let lo = (u0 * BANDS as f32 - 0.5).ceil().max(0.0) as usize;
        let hi = ((u1 * BANDS as f32 - 0.5).floor() as usize + 1).min(BANDS);
        let active = shared.range.get().hz();
        let emphasis = |j: usize| {
            let hz = bounds.0 * (bounds.1 / bounds.0).powf((j as f32 + 0.5) / BANDS as f32);
            if hz >= active.0 && hz <= active.1 {
                1.0
            } else {
                0.3
            }
        };
        let x_of = |j: usize| ((j as f32 + 0.5) / BANDS as f32 - u0) / (u1 - u0) * 2.0 - 1.0;
        let step = (((hi - lo) as f32 * POINT_PX / plane_w).ceil() as usize).max(1);

        let total = self.p.rows.max(2);
        // Parallel spectrum planes are painted far to near, including when
        // the camera rotates below or past the top of the stack.
        let mut layers: Vec<_> = self.rows.history.iter().enumerate().collect();
        layers.sort_by(|(a, _), (b, _)| {
            let depth = |i: usize| rotate(0.0, 0.0, 0.7 - 1.4 * i as f32 / (total - 1) as f32).2;
            depth(*a).total_cmp(&depth(*b))
        });
        for (i, row) in layers {
            let back = i as f32 / (total - 1) as f32;
            let z = 0.7 - 1.4 * back;
            let fade = if self.fade_speed == 0.0 {
                1.0
            } else {
                0.2 + 0.8 * (1.0 - back).powf(self.fade_speed)
            };
            let mut mesh = Mesh::default();
            let mut previous = None;
            let mut outlines = Vec::new();
            for j in (lo..hi).step_by(step) {
                let db = row[j..(j + step).min(hi)]
                    .iter()
                    .copied()
                    .fold(f32::MIN, f32::max);
                let lv = self.p.level(db);
                let centre = (j + (j + step).min(hi) - 1) / 2;
                let brightness = emphasis(centre);
                let x = x_of(centre);
                let top = project(x, lv * 0.65, z);
                let v = mesh.vertices.len() as u32;
                mesh.colored_vertex(project(x, 0.0, z), bg);
                mesh.colored_vertex(top, theme::lerp(bg, heat(lv), fade * brightness));
                if v > 0 {
                    mesh.add_triangle(v - 2, v - 1, v);
                    mesh.add_triangle(v - 1, v, v + 1);
                }
                if let Some(prev) = previous {
                    outlines.push(egui::Shape::line_segment(
                        [prev, top],
                        Stroke::new(
                            1.0,
                            theme::lerp(bg, theme::WAVE, (0.06 + 0.7 * fade) * brightness),
                        ),
                    ));
                }
                previous = Some(top);
            }
            painter.add(mesh);
            painter.extend(outlines);
        }

        // Labels follow the actual front frequency axis as the camera rotates.
        let hz = Scale::log(self.range_hz, (0.0, plane_w));
        let axis_start = project(-1.0, 0.0, 0.7);
        let axis_end = project(1.0, 0.0, 0.7);
        painter.line_segment([axis_start, axis_end], Stroke::new(1.0, plot::TICK));
        for tick in hz.ticks(64.0) {
            let point = project(hz.px(tick.value) / plane_w * 2.0 - 1.0, 0.0, 0.7);
            painter.line_segment(
                [point, point + vec2(0.0, 4.0)],
                Stroke::new(1.0, plot::TICK),
            );
            painter.text(
                point + vec2(0.0, 5.0),
                Align2::CENTER_TOP,
                plot::fmt_hz(tick.value),
                egui::FontId::monospace(9.0),
                theme::WAVE,
            );
        }
        // Time labels stay attached to the depth axis while orbiting/panning.
        let depth_ms = (total - 1) as f32 * self.p.row_ms;
        painter.line_segment(
            [axis_end, project(1.0, 0.0, -0.7)],
            Stroke::new(1.0, plot::TICK),
        );
        for back in [0.0, 0.5, 1.0] {
            let point = project(1.0, 0.0, 0.7 - 1.4 * back);
            painter.line_segment(
                [point, point + vec2(4.0, 0.0)],
                Stroke::new(1.0, plot::TICK),
            );
            painter.text(
                point + vec2(6.0, -3.0),
                Align2::LEFT_BOTTOM,
                format!("{} ms", plot::trim(back * depth_ms)),
                egui::FontId::monospace(9.0),
                theme::WAVE,
            );
        }
        plot::caption(
            &painter,
            rect,
            Align2::RIGHT_TOP,
            format!("{} ms deep", plot::trim(depth_ms)),
        );
        if ui.rect_contains_pointer(drag_rect) {
            plot::caption(
                &painter,
                rect,
                Align2::LEFT_TOP,
                format!(
                    "yaw {:.0}° · tilt {:.0}° · {:.0}%",
                    self.rotation.0,
                    self.rotation.1,
                    self.zoom * 100.0
                ),
            );
        }

        let slices = self.slices;
        let fmt = |ms: f32| {
            format!(
                "{} ms · {} slices · {} ms/slice",
                plot::trim(ms),
                slices,
                plot::trim(ms / (slices - 1) as f32)
            )
        };
        spectrum_rows::time_handle(
            ui,
            rect,
            "impulse_speed",
            &mut self.link,
            &mut self.window_ms,
            WINDOW_MS,
            DEFAULT_WINDOW_MS,
            &fmt,
        );
    }
}

impl Impulse {
    /// Derive rows and ms/row so `slices` rows span exactly `window_ms`.
    fn apply_window(&mut self) {
        self.p.rows = self.slices;
        self.p.row_ms = self.window_ms / (self.slices - 1) as f32;
    }
}

//! Impulse: a rotatable 3D waterfall (after sample_manager's stats panel): one spectrum
//! per row, newest at the front, older rows stepping back up and to the
//! right and fading into the background. Frequency has its own range;
//! the shared note range highlights the active spectrum. Shares
//! its rows and settings with the waterfall (see `spectrum_rows`).

use egui::{Align2, Color32, Mesh, Pos2, Rect, Stroke, Ui, pos2, vec2};

use super::spectrum_rows::{self, Added, BANDS, Key, Params, Rows, heat};
use super::{Module, Shared};
use crate::ui::dynamic_range;
use crate::ui::noterange::NoteRange;
use crate::ui::plot::{self, Edge, Link, Scale};
use crate::ui::theme;
use crate::ui::widgets::RangeSlider;
use crate::worker::Latest;

/// Mouse rotation around the default camera, in degrees.
const ROTATION_LIMIT: f32 = 60.0;
const DEFAULT_ROTATION: (f32, f32) = (-31.0, 0.0);
const DEFAULT_ZOOM: f32 = 1.2;
const ZOOM_LIMITS: (f32, f32) = (0.25, 8.0);
/// Room for frequency labels under the front row.
const LABEL_H: f32 = 16.0;
/// Least pixels between points along a row; denser bands are merged (peak).
const POINT_PX: f32 = 2.0;
/// Time span covered front to back, ms, and the layers per unit of depth.
const WINDOW_MS: (f32, f32) = (50.0, 4000.0);
const DEFAULT_WINDOW_MS: f32 = 500.0;
const DENSITY: (usize, usize) = (20, 100);
/// Stack depth as a multiple of the default z spacing; layers scale with it.
const DEPTH: (f32, f32) = (-2.0, 2.0);
/// Front layer's z in view space; the default stack spans ±Z_HALF.
const Z_HALF: f32 = 0.7;
/// Peak height of a full-scale spectrum, before the vertical scale.
const BASE_HEIGHT: f32 = 0.65;
/// Vertical scale of the spectrum heights; 1 = default, above 1 pushes peaks higher.
const Y_SCALE: (f32, f32) = (1.0, 3.0);

pub struct Impulse {
    p: Params,
    range_hz: (f32, f32),
    // Camera framing is session-only; only analysis parameters are persisted.
    rotation: (f32, f32),
    zoom: f32,
    /// Fade shape across the stack: 0 = solid, 1 = linear to black,
    /// below 1 holds brightness longer then drops, above 1 drops early.
    fade_curve: f32,
    /// Hue rotation towards the back layer, ±half a turn.
    hue_offset: f32,
    /// Opacity of the outline on history slices (the front slice stays solid).
    line_opacity: f32,
    /// Time from the front layer to the back one; rows and ms/row follow.
    window_ms: f32,
    /// Layers per unit of depth (rows = density × |depth|).
    density: usize,
    /// Z spacing multiplier; negative flips the stack towards the viewer.
    depth: f32,
    /// Multiplies the spectrum heights (see `Y_SCALE`).
    y_scale: f32,
    /// Pan as fractions of the viewport, so resizing preserves the framing.
    pan: (f32, f32),
    /// Temporal smoothing across layers (per band, never across bands): 0 = raw,
    /// 1 = a heavy, slowly waving blanket.
    time_smooth: f32,
    /// Smooths snapshots of the history off the UI thread; alive only
    /// while `time_smooth` is on. Eye candy, so a frame of lag is fine.
    smoother: Option<Latest<SmoothJob, Vec<Vec<f32>>>>,
    /// Newest smoothed layers, drawn instead of the raw history.
    smoothed: Vec<Vec<f32>>,
    /// The previous result's buffers, refilled for the next snapshot.
    spare: Vec<Vec<f32>>,
    /// (strength, layers) of the last snapshot sent, to resend on change.
    sent: Option<(f32, usize)>,
    rows: Rows,
    link: Link<Key>,
}

impl Default for Impulse {
    fn default() -> Self {
        Self {
            p: Params::with_rows(48),
            window_ms: DEFAULT_WINDOW_MS,
            density: 48,
            depth: 1.0,
            y_scale: 1.0,
            range_hz: (40.0, NoteRange::top_hz(false)),
            rotation: DEFAULT_ROTATION,
            zoom: DEFAULT_ZOOM,
            fade_curve: 0.5,
            hue_offset: 0.0,
            line_opacity: 1.0,
            pan: (0.0, 0.0),
            time_smooth: 0.0,
            smoother: None,
            smoothed: Vec::new(),
            spare: Vec::new(),
            sent: None,
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
        eframe::set_value(
            storage,
            &format!("{key}.fade"),
            &(self.fade_curve, self.hue_offset),
        );
        eframe::set_value(
            storage,
            &format!("{key}.window"),
            &(self.window_ms, self.density),
        );
        eframe::set_value(storage, &format!("{key}.depth"), &self.depth);
        eframe::set_value(storage, &format!("{key}.y_scale"), &self.y_scale);
        eframe::set_value(storage, &format!("{key}.time_smooth"), &self.time_smooth);
        eframe::set_value(storage, &format!("{key}.line_opacity"), &self.line_opacity);
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        if let Some((curve, hue)) = eframe::get_value::<(f32, f32)>(storage, &format!("{key}.fade"))
            && curve.is_finite()
            && hue.is_finite()
        {
            self.fade_curve = curve.clamp(0.0, 2.0);
            self.hue_offset = hue.clamp(-1.0, 1.0);
        }
        if let Some(p) = Params::load(storage, &format!("{key}.3d")) {
            self.p = p;
        }
        // Older saves only had rows and ms/row: keep the same depth.
        let (window, density) =
            eframe::get_value::<(f32, usize)>(storage, &format!("{key}.window"))
                .unwrap_or(((self.p.rows.max(2) - 1) as f32 * self.p.row_ms, self.p.rows));
        if window.is_finite() {
            self.window_ms = window.clamp(WINDOW_MS.0, WINDOW_MS.1);
        }
        self.density = density.clamp(DENSITY.0, DENSITY.1);
        if let Some(depth) = eframe::get_value::<f32>(storage, &format!("{key}.depth"))
            && depth.is_finite()
        {
            self.depth = depth.clamp(DEPTH.0, DEPTH.1);
        }
        if let Some(y) = eframe::get_value::<f32>(storage, &format!("{key}.y_scale"))
            && y.is_finite()
        {
            self.y_scale = y.clamp(Y_SCALE.0, Y_SCALE.1);
        }
        if let Some(s) = eframe::get_value::<f32>(storage, &format!("{key}.time_smooth"))
            && s.is_finite()
        {
            self.time_smooth = s.clamp(0.0, 1.0);
        }
        if let Some(o) = eframe::get_value::<f32>(storage, &format!("{key}.line_opacity"))
            && o.is_finite()
        {
            self.line_opacity = o.clamp(0.0, 1.0);
        }
        self.apply_window();
        if let Some((lo, hi)) = eframe::get_value::<(f32, f32)>(storage, &format!("{key}.range_hz"))
            && lo.is_finite()
            && hi.is_finite()
        {
            let hi = hi.clamp(41.0, NoteRange::top_hz(true));
            self.range_hz = (lo.clamp(40.0, hi - 1.0), hi);
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
        let added = self.rows.update(&self.p, shared);
        self.queue_smoothing(&added);
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        self.p.db.ui(ui, shared);
        self.link
            .slider(
                ui,
                Key::Speed,
                egui::Slider::new(&mut self.window_ms, WINDOW_MS.0..=WINDOW_MS.1)
                    .logarithmic(true)
                    .integer()
                    .text("window ms"),
            )
            .on_hover_text("Time span from the front layer to the back one.");
        ui.add(egui::Slider::new(&mut self.density, DENSITY.0..=DENSITY.1).text("density"))
            .on_hover_text("Layers per unit of depth; the layer count is density × depth.");
        ui.add(egui::Slider::new(&mut self.fade_curve, 0.0..=2.0).text("fade curve"))
            .on_hover_text(
                "0: no fade (solid stack) · 1: linear fade to black at the back · \
                 below 1 keeps the middle bright and drops off late · above 1 fades early.",
            );
        ui.add(egui::Slider::new(&mut self.time_smooth, 0.0..=1.0).text("time smooth"))
            .on_hover_text(
                "Averages each frequency band with its neighbouring layers in time \
                 (never across bands). Higher values turn the stack into a heavy, slowly \
                 waving blanket.",
            );
        self.p.ui_fft(ui);
        ui.add(
            egui::Slider::new(&mut self.depth, DEPTH.0..=DEPTH.1)
                .text("depth")
                .fixed_decimals(2),
        )
        .on_hover_text(
            "Scales the z spacing of the stack (1 = default). The layer count \
                 scales with it. Negative values flip the stack towards the viewer.",
        );
        ui.add(
            egui::Slider::new(&mut self.y_scale, Y_SCALE.0..=Y_SCALE.1)
                .text("y scale")
                .fixed_decimals(2),
        )
        .on_hover_text("Multiplies the height of the spectrum (1 = default, up to 3×).");
        ui.add(
            RangeSlider::new(
                &mut self.range_hz.0,
                &mut self.range_hz.1,
                40.0..=NoteRange::top_hz(true),
            )
            .min_gap(1.0)
            .decimals(0)
            .text("range Hz"),
        );
        self.p.ui_gate(ui);
        ui.add(egui::Slider::new(&mut self.hue_offset, -1.0..=1.0).text("hue offset"))
            .on_hover_text(
                "Rotates the hue of older layers (±½ turn at the back) and lets \
                 off-hue channels fade faster, tinting the fade.",
            );
        ui.add(egui::Slider::new(&mut self.line_opacity, 0.0..=1.0).text("line opacity"))
            .on_hover_text(
                "Opacity of the outline drawn on history slices; the front \
                 (current) slice always keeps a solid line.",
            );
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
        // Leave the right edge to the speed handle and the dB rail.
        let side = dynamic_range::beside_rail(rect);
        let drag_rect = Rect::from_min_max(plot.min, pos2(side.right() - 16.0, plot.bottom()));
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
        // Layer depth: `back` 0 is the front layer, 1 the oldest. Depth scales the spacing.
        let z_of = |back: f32| Z_HALF - 2.0 * Z_HALF * self.depth * back;
        // Peak height with the vertical scale, applied once per frame (not per point).
        let height = BASE_HEIGHT * self.y_scale;
        // Fit the volume at the base height, so the vertical scale grows the peaks
        // instead of shrinking the whole cube to make room for them.
        let mut projected = Rect::NOTHING;
        for x in [-1.0, 1.0] {
            for y in [0.0, BASE_HEIGHT] {
                for z in [z_of(0.0), z_of(1.0)] {
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

        let bounds = shared.range.get().bounds().hz();
        let band_pos = |hz: f32| (hz / bounds.0).ln() / (bounds.1 / bounds.0).ln();
        let (u0, u1) = (band_pos(self.range_hz.0), band_pos(self.range_hz.1));
        // Only include band centres inside our range (and never below 40 Hz).
        let lo = (u0 * BANDS as f32 - 0.5).ceil().max(0.0) as usize;
        let hi = ((u1 * BANDS as f32 - 0.5).floor() as usize + 1).min(BANDS);
        let active = shared.range.get().hz();
        let in_active = |j: usize| {
            let hz = bounds.0 * (bounds.1 / bounds.0).powf((j as f32 + 0.5) / BANDS as f32);
            hz >= active.0 && hz <= active.1
        };
        // Per-band membership, once per frame rather than per point per layer.
        let inside_band: Vec<bool> = (0..BANDS).map(in_active).collect();
        // Skip flat-floor points only when the floor is raised above its lowest setting.
        let at_bottom = self.p.db.range.floor <= dynamic_range::BOUNDS.0;
        let x_of_u = |u: f32| (u - u0) / (u1 - u0) * 2.0 - 1.0;
        let x_of = |j: usize| x_of_u((j as f32 + 0.5) / BANDS as f32);
        // The depth axis parks on the end of the filled (active) region nearer
        // the camera (larger view depth is nearer, see the layer sort): the high
        // end at the default yaw, the low end once yaw turns it away.
        let across = (project(1.0, 0.0, z_of(0.0)) - project(-1.0, 0.0, z_of(0.0))).x;
        let high = rotate(1.0, 0.0, 0.0).2 >= rotate(-1.0, 0.0, 0.0).2 - 1e-3;
        let fill_x = x_of_u(band_pos(if high { active.1 } else { active.0 })).clamp(-1.0, 1.0);
        // Screen direction pointing away from the fill, for ticks and labels.
        let out = if high == (across >= 0.0) { 1.0 } else { -1.0 };
        let step = (((hi - lo) as f32 * POINT_PX / plane_w).ceil() as usize).max(1);

        let total = self.p.rows.max(2);
        // Smoothed layers lag the history by a frame or so; raw until the first arrives.
        let source: Vec<&[f32]> = if self.time_smooth > 0.0 && !self.smoothed.is_empty() {
            self.smoothed.iter().take(total).map(Vec::as_slice).collect()
        } else {
            self.rows.history.iter().map(Vec::as_slice).collect()
        };
        // Parallel spectrum planes are painted far to near, including when
        // the camera rotates below or past the top of the stack.
        let mut layers: Vec<_> = source.into_iter().enumerate().collect();
        layers.sort_by(|(a, _), (b, _)| {
            let depth_of = |i: usize| rotate(0.0, 0.0, z_of(i as f32 / (total - 1) as f32)).2;
            depth_of(*a).total_cmp(&depth_of(*b))
        });
        for (i, row) in layers {
            let back = i as f32 / (total - 1) as f32;
            let z = z_of(back);
            let fade = if self.fade_curve <= 0.0 {
                1.0
            } else {
                1.0 - back.powf(1.0 / self.fade_curve)
            };
            let turns = self.hue_offset * back * 0.5;
            let tint = 2.0 * self.hue_offset.abs();
            let mut mesh = Mesh::default();
            // Outside the active note range: no fill; only the current slice
            // keeps a faint white trace there, travelling slices skip it.
            let current = i == 0;
            let outside_stroke = Stroke::new(1.0, Color32::WHITE.gamma_multiply(0.2 * fade));
            let line_alpha = if current { 1.0 } else { self.line_opacity };
            let inside_stroke = Stroke::new(
                1.0,
                theme::hue_fade(bg, theme::WAVE, turns, 0.06 + 0.7 * fade, tint)
                    .gamma_multiply(line_alpha),
            );
            // (top point, its vertex index, inside the active range)
            let mut previous: Option<(Pos2, u32, bool)> = None;
            // Outline: consecutive segments sharing a stroke form one polyline
            // (one shape per run instead of one per segment).
            let mut outlines = Vec::new();
            let mut run: Vec<Pos2> = Vec::new();
            let mut run_stroke = inside_stroke;
            for j in (lo..hi).step_by(step) {
                let db = row[j..(j + step).min(hi)]
                    .iter()
                    .copied()
                    .fold(f32::MIN, f32::max);
                let lv = self.p.level(db);
                if lv <= 0.0 && !at_bottom {
                    // Flat floor: no fill or outline. The next non-zero point starts a new run.
                    // At the lowest floor setting, flat points are drawn as before.
                    previous = None;
                    push_run(&mut outlines, &mut run, run_stroke);
                    continue;
                }
                let centre = (j + (j + step).min(hi) - 1) / 2;
                let inside = inside_band[centre];
                let x = x_of(centre);
                let top = project(x, lv * height, z);
                let v = mesh.vertices.len() as u32;
                if inside {
                    mesh.colored_vertex(project(x, 0.0, z), bg);
                    mesh.colored_vertex(top, theme::hue_fade(bg, heat(lv), turns, fade, tint));
                }
                if let Some((prev, prev_v, prev_inside)) = previous {
                    let both = inside && prev_inside;
                    if both {
                        mesh.add_triangle(prev_v, prev_v + 1, v);
                        mesh.add_triangle(prev_v + 1, v, v + 1);
                    }
                    let stroke = if both {
                        Some(inside_stroke)
                    } else if current {
                        Some(outside_stroke)
                    } else {
                        None
                    };
                    match stroke {
                        Some(s) => {
                            if run.last() != Some(&prev) || s != run_stroke {
                                push_run(&mut outlines, &mut run, run_stroke);
                                run.push(prev);
                                run_stroke = s;
                            }
                            run.push(top);
                        }
                        None => push_run(&mut outlines, &mut run, run_stroke),
                    }
                }
                previous = Some((top, v, inside));
            }
            push_run(&mut outlines, &mut run, run_stroke);
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
        // Time labels stay attached to the depth axis while orbiting/panning;
        // labels sit on the side away from the fill.
        let depth_ms = (total - 1) as f32 * self.p.row_ms;
        painter.line_segment(
            [
                project(fill_x, 0.0, z_of(0.0)),
                project(fill_x, 0.0, z_of(1.0)),
            ],
            Stroke::new(1.0, plot::TICK),
        );
        for back in [0.0, 0.5, 1.0] {
            let point = project(fill_x, 0.0, z_of(back));
            painter.line_segment(
                [point, point + vec2(4.0 * out, 0.0)],
                Stroke::new(1.0, plot::TICK),
            );
            painter.text(
                point + vec2(6.0 * out, -3.0),
                if out > 0.0 {
                    Align2::LEFT_BOTTOM
                } else {
                    Align2::RIGHT_BOTTOM
                },
                format!("{} ms", plot::trim(back * depth_ms)),
                egui::FontId::monospace(9.0),
                theme::WAVE,
            );
        }
        plot::caption(
            &painter,
            side,
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

        let count = self.p.rows.max(2);
        let fmt = |ms: f32| {
            format!(
                "{} ms · {} layers · {} ms/layer",
                plot::trim(ms),
                count,
                plot::trim(ms / (count - 1) as f32)
            )
        };
        spectrum_rows::time_handle(
            ui,
            side,
            spectrum_rows::TimeHandle {
                id: "impulse_speed",
                edge: Edge::Right,
                range: WINDOW_MS,
                reset: DEFAULT_WINDOW_MS,
                fmt: &fmt,
            },
            &mut self.link,
            &mut self.window_ms,
        );
        self.p.db.rail(ui, rect, "impulse_db", shared);
    }
}

impl Impulse {
    /// Derive rows and ms/row: layers = density × |depth|, spanning exactly `window_ms`.
    fn apply_window(&mut self) {
        let layers = (self.density as f32 * self.depth.abs()).round().max(2.0) as usize;
        self.p.rows = layers;
        self.p.row_ms = self.window_ms / (layers - 1) as f32;
    }

    /// Collect the newest smoothed layers and, when the history or the
    /// strength changed, send the worker a fresh snapshot of it.
    fn queue_smoothing(&mut self, added: &Added) {
        if self.time_smooth <= 0.0 {
            // Dropping the worker ends its thread.
            self.smoother = None;
            self.smoothed.clear();
            self.sent = None;
            return;
        }
        let worker = self.smoother.get_or_insert_with(|| {
            Latest::spawn("impulse smooth", |mut job: SmoothJob| {
                smooth_time(&mut job.rows, job.strength, job.total);
                job.rows
            })
        });
        if let Some(done) = worker.take() {
            self.spare = std::mem::replace(&mut self.smoothed, done);
        }
        let key = (self.time_smooth, self.p.rows.max(2));
        if added.rows == 0 && !added.cleared && self.sent == Some(key) {
            return;
        }
        self.sent = Some(key);
        let history = &self.rows.history;
        let mut rows = std::mem::take(&mut self.spare);
        rows.resize_with(history.len(), Vec::new);
        for (dst, src) in rows.iter_mut().zip(history) {
            dst.clone_from(src);
        }
        worker.submit(SmoothJob {
            rows,
            strength: key.0,
            total: key.1,
        });
    }
}

/// A snapshot of the history (newest first) to smooth in place.
struct SmoothJob {
    rows: Vec<Vec<f32>>,
    strength: f32,
    total: usize,
}

/// Emit a run of connected points as one polyline (if it has a segment), then clear it.
fn push_run(out: &mut Vec<egui::Shape>, run: &mut Vec<Pos2>, stroke: Stroke) {
    if run.len() >= 2 {
        out.push(egui::Shape::line(std::mem::take(run), stroke));
    } else {
        run.clear();
    }
}

/// Zero-phase temporal smoothing of `rows` in place: per band, a one-pole
/// low-pass runs forward then backward across the layers (twice, for a
/// near-Gaussian kernel). Cost is linear in layers × bands whatever the
/// strength. The time constant is relative to the stack size, so the blanket
/// looks the same at any density; bands are never mixed. The window (time
/// constant) grows continuously with age, `tau_max · t^p`; strength both
/// widens `tau_max` and lowers `p`, so the growth bends towards the front:
/// ~0.3 gets blurry from about half, ~0.7 keeps detail only in the first few
/// layers, 1 falls off steeply into the blanket.
fn smooth_time(rows: &mut [Vec<f32>], strength: f32, total: usize) {
    for db in rows.iter_mut().flatten() {
        *db = db.max(-200.0);
    }
    let n = rows.len();
    if strength <= 0.0 || n < 2 {
        return;
    }
    // Perceived blur is roughly logarithmic in the kernel width, so the
    // slider sweeps σ at the back layer exponentially from ~0.25 layer up to
    // half the stack; the `- MIN` keeps 0 exactly unblurred.
    const MIN: f32 = 0.25;
    let max = (0.5 * total as f32).max(MIN * 2.0);
    let sigma_back = MIN * (max / MIN).powf(strength) - MIN * (1.0 - strength);
    // σ grows linearly from the live (raw) front layer to the back one, and
    // continuously, so there is no seam between lightly and heavily blurred
    // layers. Three box passes of width w approximate a Gaussian with
    // σ² = 3·w²/12; w = √(1 + 4σ²) starts at a single layer (identity).
    let width: Vec<f32> = (0..n)
        .map(|i| {
            let sigma = sigma_back * i as f32 / (n - 1) as f32;
            (1.0 + 4.0 * sigma * sigma).sqrt()
        })
        .collect();
    let bands = rows[0].len();
    let mut prefix = vec![0.0f32; (n + 1) * bands];
    let mut fresh = vec![0.0f32; n * bands];
    for _ in 0..3 {
        box_pass(rows, &width, &mut prefix, &mut fresh, bands);
    }
}

/// One variable-width box blur along time, per band. Each row is a unit cell
/// [i, i+1); row i becomes the mean over [i+½−w/2, i+½+w/2] (clamped to the
/// stack), with fractional edges so the width varies smoothly.
fn box_pass(
    rows: &mut [Vec<f32>],
    width: &[f32],
    prefix: &mut [f32],
    fresh: &mut [f32],
    bands: usize,
) {
    let n = rows.len();
    for (i, row) in rows.iter().enumerate() {
        let (done, rest) = prefix.split_at_mut((i + 1) * bands);
        let prev = &done[i * bands..];
        for ((p, &prev), &v) in rest[..bands].iter_mut().zip(prev).zip(row.iter()) {
            *p = prev + v;
        }
    }
    // Integral of the piecewise-constant rows from 0 to x, for band b.
    let integral = |rows: &[Vec<f32>], x: f32, b: usize| {
        let k = (x.floor() as usize).min(n - 1);
        prefix[k * bands + b] + (x - k as f32) * rows[k][b]
    };
    for (i, (&w, out)) in width.iter().zip(fresh.chunks_exact_mut(bands)).enumerate() {
        let c = i as f32 + 0.5;
        let (a, b) = ((c - 0.5 * w).max(0.0), (c + 0.5 * w).min(n as f32));
        for (band, f) in out.iter_mut().enumerate() {
            *f = (integral(rows, b, band) - integral(rows, a, band)) / (b - a);
        }
    }
    for (dst, src) in rows.iter_mut().zip(fresh.chunks_exact(bands)) {
        dst.copy_from_slice(src);
    }
}

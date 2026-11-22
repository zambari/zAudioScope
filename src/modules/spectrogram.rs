//! Rolling spectrogram, newest row at the top. The frequency axis is the
//! shared note range (log frequency, set in the bar above the status bar):
//! rows are kept across the bar's whole span and the texture is cropped to
//! the selection, so changing the range keeps the history. The handle on
//! the right sets the scroll speed.

use std::collections::VecDeque;

use egui::{Color32, ColorImage, Rect, TextureHandle, TextureOptions, Ui, pos2};

use super::spectrum_rows::{BANDS, Key, Params, Rows, visible_bands};
use super::{Module, Shared};
use crate::ui::plot::{self, Axis, Edge, Link, Scale};

#[derive(Default)]
pub struct Spectrogram {
    p: Params,
    rows: Rows,
    /// The rows as colours, made once per row instead of every frame.
    colored: VecDeque<Vec<Color32>>,
    /// (floor, ceiling, rows) the colours and texture were made with.
    colored_for: (f32, f32, usize),
    /// The texture is out of date.
    dirty: bool,
    texture: Option<TextureHandle>,
    link: Link<Key>,
}

impl Module for Spectrogram {
    fn title(&self) -> &'static str {
        "Spectrogram"
    }

    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str) {
        eframe::set_value(storage, key, &self.p);
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        // Saved as "Waterfall" before the 3D waterfall took that name.
        let legacy = key.replace(self.title(), "Waterfall");
        if let Some(p) = Params::load(storage, key, Some(&legacy)) {
            self.p = p;
        }
    }

    fn lookback(&self, rate: u32) -> usize {
        self.p.lookback(rate)
    }

    fn update(&mut self, shared: &mut Shared) {
        self.p.db.sync(&shared.db);
        let added = self.rows.update(&self.p, shared);
        if added.cleared {
            self.colored.clear();
        }
        for row in self.rows.history.iter().take(added.rows).rev() {
            self.colored.push_front(row.iter().map(|&db| self.p.color(db)).collect());
        }
        self.colored.truncate(self.p.rows);
        self.dirty |= added.cleared || added.rows > 0;
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        self.p.ui(ui, shared, &mut self.link);
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        if self.rows.history.is_empty() {
            return;
        }
        // Colour scale changed: recolour what we have.
        let key = (self.p.db.range.floor, self.p.db.range.ceil, self.p.rows);
        if self.colored_for != key {
            self.colored_for = key;
            self.colored = self.rows.history.iter().map(|r| r.iter().map(|&db| self.p.color(db)).collect()).collect();
            self.dirty = true;
        }
        if self.texture.is_none() || self.dirty {
            self.dirty = false;
            let mut img = ColorImage::filled([BANDS, self.p.rows], Color32::from_gray(13));
            for (y, row) in self.colored.iter().take(self.p.rows).enumerate() {
                let w = row.len().min(BANDS);
                img.pixels[y * BANDS..y * BANDS + w].copy_from_slice(&row[..w]);
            }
            match &mut self.texture {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(ui.ctx().load_texture("spectrogram", img, TextureOptions::LINEAR)),
            }
        }
        self.paint(ui, rect, shared);
    }
}

impl Spectrogram {
    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        let Some(tex) = self.texture.as_ref().map(|t| t.id()) else { return };
        let painter = ui.painter_at(rect);
        let range = shared.range.get();
        let (_, (u0, u1)) = visible_bands(range);
        painter.image(tex, rect, Rect::from_min_max(pos2(u0, 0.0), pos2(u1, 1.0)), Color32::WHITE);

        // Frequency along the bottom, time ago down the left.
        Axis::new(Edge::Bottom, range.hz_scale(rect)).spacing(48.0).draw(&painter, rect, plot::fmt_hz);
        Axis::new(Edge::Left, Scale::linear((0.0, self.p.secs()), (rect.top(), rect.bottom())))
            .spacing(32.0)
            .grid(false)
            .draw(&painter, rect, plot::fmt_secs);

        self.p.speed_handle(ui, rect, "spectrogram_speed", &mut self.link);
    }
}

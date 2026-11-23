//! Rolling waterfall, newest row at the top. The frequency axis is the
//! shared note range (log frequency, set in the bar above the status bar):
//! rows are kept across the bar's whole span and the texture is cropped to
//! the selection, so changing the range keeps the history. The handle on
//! the right sets the scroll speed; the dB rail beside it the colour scale.

use egui::{Color32, ColorImage, Rect, TextureHandle, TextureOptions, Ui, pos2};

use super::spectrum_rows::{BANDS, Key, Palette, Params, Rows, visible_bands};
use super::{Module, Shared};
use crate::ui::dynamic_range;
use crate::ui::plot::{self, Axis, Edge, Link, Scale};

/// Colour of rows not yet filled.
const BLANK: Color32 = Color32::from_gray(13);

/// Colour-scale key: what the coloured rows depend on.
type ColourKey = (f32, f32, usize, Palette);

/// The coloured rows as a ring of slots, one slot per texture row. The
/// newest row is in slot `head - 1`. A new row is one slot to upload, not
/// the whole image.
#[derive(Default)]
struct Ring {
    pixels: Vec<Color32>,
    slots: usize,
    /// Next slot to write.
    head: usize,
    /// Slots written since the texture last took them.
    pending: Vec<usize>,
    /// The whole ring must be re-uploaded.
    full: bool,
}

impl Ring {
    /// Empty ring of `slots` rows; the texture needs a full upload.
    fn reset(&mut self, slots: usize) {
        self.slots = slots;
        self.pixels.clear();
        self.pixels.resize(slots * BANDS, BLANK);
        self.head = 0;
        self.pending.clear();
        self.full = true;
    }

    /// Colour one row into the next slot.
    fn put(&mut self, row: &[f32], p: &Params) {
        let slot = self.head;
        for (dst, &db) in self.pixels[slot * BANDS..(slot + 1) * BANDS]
            .iter_mut()
            .zip(row)
        {
            *dst = p.color(db);
        }
        self.head = (self.head + 1) % self.slots;
        self.pending.push(slot);
        if self.pending.len() > self.slots {
            // Nothing has drawn for a while: one full upload is cheaper.
            self.pending.clear();
            self.full = true;
        }
    }
}

#[derive(Default)]
pub struct Waterfall {
    p: Params,
    rows: Rows,
    ring: Ring,
    /// The colour scale the ring was coloured with.
    coloured_for: Option<ColourKey>,
    /// Rows the texture was made with.
    tex_rows: usize,
    texture: Option<TextureHandle>,
    link: Link<Key>,
}

impl Module for Waterfall {
    fn title(&self) -> &'static str {
        "Waterfall"
    }

    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str) {
        eframe::set_value(storage, key, &self.p);
    }

    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str) {
        if let Some(p) = Params::load(storage, key) {
            self.p = p;
        }
    }

    fn lookback(&self, rate: u32) -> usize {
        self.p.lookback(rate)
    }

    /// Keep drawing until the last loud row has scrolled off the bottom.
    fn idle_wait_s(&self) -> f32 {
        crate::ui::meter::NO_SIGNAL_HOLD_S.max(self.p.secs())
    }

    fn update(&mut self, shared: &mut Shared) {
        self.p.db.sync(&shared.db);
        let added = self.rows.update(&self.p, shared);
        let key = self.colour_key();
        if added.cleared || self.coloured_for != Some(key) {
            self.recolour(key);
        } else {
            for row in self.rows.history.iter().take(added.rows).rev() {
                self.ring.put(row, &self.p);
            }
        }
    }

    fn params(&mut self, ui: &mut Ui, shared: &Shared) {
        egui::ComboBox::from_label("palette")
            .selected_text(self.p.palette.name())
            .show_ui(ui, |ui| {
                for palette in Palette::ALL {
                    ui.selectable_value(&mut self.p.palette, palette, palette.name());
                }
            });
        self.p.ui(ui, shared, &mut self.link);
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, true);
        self.p.db.rail(ui, rect, "waterfall_db", shared);
    }

    fn layer_depth(&self) -> Option<u8> {
        Some(0)
    }

    fn layer(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.paint(ui, rect, shared, false);
    }
}

impl Waterfall {
    fn colour_key(&self) -> ColourKey {
        (
            self.p.db.range.floor,
            self.p.db.range.ceil,
            self.p.rows,
            self.p.palette,
        )
    }

    /// Colour every retained row afresh (new scale, size or history).
    fn recolour(&mut self, key: ColourKey) {
        self.coloured_for = Some(key);
        self.ring.reset(self.p.rows);
        // Oldest first, so the newest row ends up just before `head`.
        for row in self.rows.history.iter().rev() {
            self.ring.put(row, &self.p);
        }
    }

    /// Bring the texture up to date with the ring.
    fn refresh(&mut self, ui: &Ui) {
        let ring = &mut self.ring;
        if !ring.full && self.tex_rows == ring.slots {
            if let Some(t) = self.texture.as_mut() {
                for slot in ring.pending.drain(..) {
                    let row = ring.pixels[slot * BANDS..(slot + 1) * BANDS].to_vec();
                    let img = ColorImage::new([BANDS, 1], row);
                    t.set_partial([0, slot], img, TextureOptions::LINEAR);
                }
                return;
            }
        }
        let img = ColorImage::new([BANDS, ring.slots], ring.pixels.clone());
        ring.pending.clear();
        ring.full = false;
        self.tex_rows = self.ring.slots;
        match &mut self.texture {
            Some(t) => t.set(img, TextureOptions::LINEAR),
            None => {
                self.texture = Some(
                    ui.ctx()
                        .load_texture("waterfall", img, TextureOptions::LINEAR),
                )
            }
        }
    }

    /// `full`: with axes and the speed handle; otherwise the image alone.
    fn paint(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared, full: bool) {
        self.link.tick(ui.input(|i| i.stable_dt).clamp(0.0, 0.1));
        if self.rows.history.is_empty() || self.ring.slots == 0 {
            return;
        }
        self.refresh(ui);
        let Some(tex) = self.texture.as_ref().map(|t| t.id()) else {
            return;
        };
        let painter = ui.painter_at(rect);
        let range = shared.range.get();
        let (_, (u0, u1)) = visible_bands(range);

        // Screen top is the newest row, in slot `head - 1`, and it runs down
        // through the lower slots to 0. Then it wraps to the top slot and runs
        // down to `head`. Each piece is a run of slots in reverse, so it is
        // drawn with its UVs flipped vertically.
        let head = self.ring.head;
        let split = head as f32 / self.ring.slots as f32;
        let h = rect.height();
        let newest = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + h * split));
        let oldest = Rect::from_min_max(pos2(rect.left(), rect.top() + h * split), rect.max);
        if head > 0 {
            painter.image(
                tex,
                newest,
                Rect::from_min_max(pos2(u0, split), pos2(u1, 0.0)),
                Color32::WHITE,
            );
        }
        if head < self.ring.slots {
            painter.image(
                tex,
                oldest,
                Rect::from_min_max(pos2(u0, 1.0), pos2(u1, split)),
                Color32::WHITE,
            );
        }
        if !full {
            return;
        }

        // Frequency along the bottom, time ago down the left.
        Axis::new(Edge::Bottom, range.hz_scale(rect))
            .spacing(48.0)
            .draw(&painter, rect, plot::fmt_hz);
        Axis::new(
            Edge::Left,
            Scale::linear((0.0, self.p.secs()), (rect.top(), rect.bottom())),
        )
        .spacing(32.0)
        .grid(false)
        .draw(&painter, rect, plot::fmt_secs);

        self.p.fall_handle(
            ui,
            dynamic_range::beside_rail(rect),
            "waterfall_fall",
            &mut self.link,
        );
    }
}

//! The dB floor / ceiling control every level-scaled view uses (tuner
//! spectrum, waterfall, Impulse), with its limits defined once here.
//!
//! Each control has its own persisted range plus a padlock. Locked controls
//! all read and write one shared range ([`Shared::db`], kept by the app), so
//! dragging any of them moves every locked view; unlocked ones are left
//! alone. There is no master: locking a control pushes its current range to
//! the shared one, and unlocking keeps whatever the shared range was.
//!
//! [`Shared::db`]: crate::modules::Shared

use std::cell::Cell;

use egui::{Rect, Response, Sense, Ui, vec2};
use serde::{Deserialize, Serialize};

use super::theme::{VU_CLIP, WAVE};
use super::widgets::RangeSlider;
use crate::audio::levels::LevelDist;
use crate::modules::Shared;

/// Lowest floor and highest ceiling any control allows, dB. The single
/// source of truth: every control starts at, and is clamped to, this span.
pub const BOUNDS: (f32, f32) = (-60.0, 4.0);
/// Smallest floor-to-ceiling span, dB.
pub const MIN_GAP: f32 = 12.0;

/// A floor / ceiling pair, the value the locked controls share.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct DbRange {
    pub floor: f32,
    pub ceil: f32,
}

impl Default for DbRange {
    fn default() -> Self {
        Self { floor: BOUNDS.0, ceil: BOUNDS.1 }
    }
}

impl DbRange {
    pub fn new(floor: f32, ceil: f32) -> Self {
        Self { floor, ceil }.sanitized()
    }

    /// Inside [`BOUNDS`] with at least [`MIN_GAP`] between the ends.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let floor = if self.floor.is_finite() { self.floor } else { d.floor };
        let ceil = if self.ceil.is_finite() { self.ceil } else { d.ceil };
        let floor = floor.clamp(BOUNDS.0, BOUNDS.1 - MIN_GAP);
        Self { floor, ceil: ceil.clamp(floor + MIN_GAP, BOUNDS.1) }
    }

    /// Floor lifted off its minimum, so anything quieter is hidden.
    pub fn floor_raised(&self) -> bool {
        self.floor > BOUNDS.0
    }

    /// 0 at the floor, 1 at the ceiling, clamped.
    pub fn level(&self, db: f32) -> f32 {
        ((db - self.floor) / (self.ceil - self.floor)).clamp(0.0, 1.0)
    }
}

/// One module's control: its own range and whether it follows the shared one.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct DynamicRange {
    pub range: DbRange,
    pub locked: bool,
}

impl Default for DynamicRange {
    /// The full [`BOUNDS`] span, unlocked.
    fn default() -> Self {
        Self { range: DbRange::default(), locked: false }
    }
}

impl DynamicRange {

    pub fn sanitize(&mut self) {
        self.range = self.range.sanitized();
    }

    /// Take a legacy flat `floor_db` / `ceil_db` pair if one was loaded
    /// (finite), then reset it to NaN so it's read only once.
    pub fn migrate(&mut self, floor: &mut f32, ceil: &mut f32) {
        if floor.is_finite() && ceil.is_finite() {
            self.range = DbRange::new(*floor, *ceil);
        }
        (*floor, *ceil) = (f32::NAN, f32::NAN);
    }

    /// While locked, follow the shared range. Call once per frame before
    /// drawing with [`Self::range`].
    pub fn sync(&mut self, hub: &Cell<DbRange>) {
        if self.locked {
            self.range = hub.get();
        }
    }

    pub fn level(&self, db: f32) -> f32 {
        self.range.level(db)
    }

    /// Padlock plus the two-handle slider, with a thin meter row under the
    /// slider: from the left, how much of the recent audio sits below the
    /// floor; from the right, how much pokes above the ceiling.
    pub fn ui(&mut self, ui: &mut Ui, shared: &Shared) -> Response {
        let hub = &shared.db;
        let r = ui
            .horizontal(|ui| {
                let was = self.locked;
                let icon = if was { "🔒" } else { "🔓" };
                ui.toggle_value(&mut self.locked, icon)
                    .on_hover_text("Lock: share this dB range with every other locked view");
                if self.locked && !was {
                    hub.set(self.range);
                }
                self.sync(hub);
                let DbRange { floor, ceil } = &mut self.range;
                let r = ui.add(RangeSlider::new(floor, ceil, BOUNDS.0..=BOUNDS.1).min_gap(MIN_GAP).text("dB"));
                if self.locked {
                    hub.set(self.range);
                }
                r
            })
            .inner;
        coverage_row(ui, r.rect, self.range, &shared.levels);
        r
    }
}

/// Height of the floor / ceiling coverage meter, px.
const ROW_H: f32 = 3.0;

/// The 3 px meter under a range slider, spanning `under`'s width. Bar
/// lengths are √fraction so a few percent clipping is still visible.
fn coverage_row(ui: &mut Ui, under: Rect, range: DbRange, levels: &LevelDist) {
    let gap = ui.spacing().item_spacing.y;
    ui.add_space(1.0 - gap);
    let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let row = Rect::from_x_y_ranges(under.x_range(), row.y_range());
    let (low, high) = (levels.below(range.floor), levels.above(range.ceil));
    if ui.is_rect_visible(row) {
        let painter = ui.painter();
        painter.rect_filled(row, 0.0, ui.visuals().extreme_bg_color);
        let w = row.width();
        painter.rect_filled(Rect::from_x_y_ranges(row.left()..=row.left() + w * low.sqrt(), row.y_range()), 0.0, WAVE.gamma_multiply(0.6));
        painter.rect_filled(Rect::from_x_y_ranges(row.right() - w * high.sqrt()..=row.right(), row.y_range()), 0.0, VU_CLIP);
    }
    resp.on_hover_text(format!(
        "Last few seconds of input, 20 ms RMS blocks:\n{:.0}% below the floor (left)\n{:.1}% above the ceiling (right)",
        low * 100.0,
        high * 100.0
    ));
}

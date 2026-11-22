//! The dB floor / ceiling control every level-scaled view uses (tuner
//! spectrum, spectrogram, 3D waterfall), with its limits defined once here.
//!
//! Each control has its own persisted range plus a padlock. Locked controls
//! all read and write one shared range ([`Shared::db`], kept by the app), so
//! dragging any of them moves every locked view; unlocked ones are left
//! alone. There is no master: locking a control pushes its current range to
//! the shared one, and unlocking keeps whatever the shared range was.
//!
//! [`Shared::db`]: crate::modules::Shared

use std::cell::Cell;

use egui::{Response, Ui};
use serde::{Deserialize, Serialize};

use super::widgets::RangeSlider;

/// Lowest floor and highest ceiling any control allows, dB.
pub const BOUNDS: (f32, f32) = (-140.0, 6.0);
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
        Self { floor: -96.0, ceil: -12.0 }
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
    fn default() -> Self {
        Self::new(DbRange::default().floor, DbRange::default().ceil)
    }
}

impl DynamicRange {
    pub fn new(floor: f32, ceil: f32) -> Self {
        Self { range: DbRange::new(floor, ceil), locked: false }
    }

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

    /// Padlock plus the two-handle slider.
    pub fn ui(&mut self, ui: &mut Ui, hub: &Cell<DbRange>) -> Response {
        ui.horizontal(|ui| {
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
        .inner
    }
}

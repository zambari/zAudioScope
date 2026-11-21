//! Pitch detector / tuner. Snaps the detected pitch to the nearest enabled
//! note (or the nearest semitone when none are enabled) and says which way
//! and how far to turn the device's tune control.

use egui::{Align2, FontId, Rect, Stroke, Ui};

use super::{Module, Shared};
use crate::pitch::{self, Method};
use crate::ui::theme;

const OCTAVES: usize = 3;
/// Longest stretch fed to the detector; keeps the per-frame cost bounded.
const MAX_ANALYSIS: usize = 4096;

pub struct Tuner {
    method: Method,
    /// 0 = no smoothing, towards 1 = very slow.
    smoothing: f32,
    a4: f32,
    /// Device tune-control units per semitone; calibrate against the
    /// outboard gear ("0.06 per ~6 cents" → 1.0).
    units_per_semitone: f32,
    /// MIDI note of the lowest toggle (C1 = 24).
    base_midi: i32,
    enabled: [bool; 12 * OCTAVES],
    f_min: f32,
    f_max: f32,
    /// Smoothed fractional MIDI note, `None` when silent.
    midi: Option<f32>,
}

impl Default for Tuner {
    fn default() -> Self {
        Self {
            method: Method::Autocorrelation,
            smoothing: 0.8,
            a4: 440.0,
            units_per_semitone: 1.0,
            base_midi: 24,
            enabled: [false; 12 * OCTAVES],
            f_min: 28.0,
            f_max: 600.0,
            midi: None,
        }
    }
}

impl Tuner {
    fn enabled_notes(&self) -> Vec<i32> {
        let on: Vec<i32> =
            (0..self.enabled.len()).filter(|&i| self.enabled[i]).map(|i| self.base_midi + i as i32).collect();
        if on.is_empty() { (0..128).collect() } else { on }
    }
}

impl Module for Tuner {
    fn title(&self) -> &'static str {
        "Tuner"
    }

    fn graph_height(&self) -> f32 {
        170.0
    }

    fn update(&mut self, shared: &mut Shared) {
        let x = shared.analysis_slice(MAX_ANALYSIS);
        let x = &x[..x.len().min(MAX_ANALYSIS)];
        let detected = pitch::detect(self.method, x, shared.frame.rate, self.f_min, self.f_max)
            .map(|f| pitch::freq_to_midi(f, self.a4));
        self.midi = match (self.midi, detected) {
            // Jumped by more than a semitone: a new note, don't glide to it.
            (Some(prev), Some(m)) if (m - prev).abs() < 1.0 => Some(prev * self.smoothing + m * (1.0 - self.smoothing)),
            (_, m) => m,
        };
    }

    fn params(&mut self, ui: &mut Ui) {
        egui::ComboBox::from_id_salt("tuner_method").selected_text(self.method.label()).show_ui(ui, |ui| {
            for m in Method::ALL {
                ui.selectable_value(&mut self.method, m, m.label());
            }
        });
        ui.add(egui::Slider::new(&mut self.smoothing, 0.0..=0.98).text("smoothing"));
        ui.add(egui::Slider::new(&mut self.a4, 415.0..=466.0).text("A4 Hz"));
        ui.add(egui::Slider::new(&mut self.units_per_semitone, 0.01..=10.0).logarithmic(true).text("units/semi"));
        ui.horizontal(|ui| {
            ui.label("from");
            if ui.small_button("−").clicked() {
                self.base_midi = (self.base_midi - 12).max(12);
            }
            ui.label(pitch::note_name(self.base_midi));
            if ui.small_button("+").clicked() {
                self.base_midi = (self.base_midi + 12).min(84);
            }
            if ui.small_button("none").clicked() {
                self.enabled = [false; 12 * OCTAVES];
            }
        });
        // One row per octave, one tiny toggle per note.
        ui.spacing_mut().item_spacing = egui::vec2(1.0, 1.0);
        for oct in 0..OCTAVES {
            ui.horizontal(|ui| {
                for n in 0..12 {
                    let i = oct * 12 + n;
                    let name = pitch::NOTE_NAMES[n];
                    let label = egui::RichText::new(name).size(9.0);
                    let b = egui::Button::selectable(self.enabled[i], label).min_size(egui::vec2(17.0, 15.0));
                    if ui.add(b).on_hover_text(pitch::note_name(self.base_midi + i as i32)).clicked() {
                        self.enabled[i] = !self.enabled[i];
                    }
                }
            });
        }
    }

    fn graph(&mut self, ui: &mut Ui, rect: Rect, _shared: &Shared) {
        let painter = ui.painter_at(rect);
        let Some(midi) = self.midi else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "—", FontId::proportional(40.0), ui.visuals().weak_text_color());
            return;
        };
        let Some((target, cents)) = pitch::snap(midi, self.enabled_notes()) else { return };

        // Needle: ±50 cents across the width.
        let x = rect.center().x + (cents / 50.0).clamp(-1.0, 1.0) * rect.width() * 0.45;
        let in_tune = cents.abs() < 3.0;
        let col = if in_tune { theme::GOOD } else { theme::ACCENT_BRIGHT };
        painter.vline(rect.center().x, rect.y_range(), Stroke::new(1.0, ui.visuals().weak_text_color()));
        painter.vline(x, rect.y_range(), Stroke::new(3.0, col));

        let units = cents / 100.0 * self.units_per_semitone;
        let advice = if in_tune {
            "in tune".to_owned()
        } else if cents > 0.0 {
            format!("▼ tune down {:.2}", units.abs())
        } else {
            format!("▲ tune up {:.2}", units.abs())
        };
        let hz = pitch::midi_to_freq(midi, self.a4);
        painter.text(
            rect.center_top() + egui::vec2(0.0, 10.0),
            Align2::CENTER_TOP,
            pitch::note_name(target),
            FontId::proportional(44.0),
            col,
        );
        painter.text(rect.center(), Align2::CENTER_TOP, &advice, FontId::proportional(26.0), col);
        painter.text(
            rect.left_bottom() + egui::vec2(8.0, -6.0),
            Align2::LEFT_BOTTOM,
            format!("{hz:.2} Hz   {cents:+.1} ¢"),
            FontId::monospace(12.0),
            ui.visuals().text_color(),
        );
    }
}

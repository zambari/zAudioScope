//! Window layout: top bar (input device, module toggles), a scroll view of
//! foldout analyser rows, and a status bar.

use serde::{Deserialize, Serialize};

use crate::audio::{DeviceInfo, Input};
use crate::modules::{self, Gate, Shared, Slot};
use crate::ui;

const STATE_KEY: &str = "zaudioscope_state";
/// Seconds of audio handed to the modules each frame.
const FRAME_SECS: f32 = 2.0;

/// What survives a restart.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    device_id: Option<String>,
    /// Per module title: (visible, open).
    modules: Vec<(String, bool, bool)>,
}

pub struct App {
    input: Input,
    devices: Vec<DeviceInfo>,
    slots: Vec<Slot>,
    level: f32,
}

impl App {
    pub fn new(ctx: &egui::Context, storage: Option<&dyn eframe::Storage>) -> Self {
        ui::theme::apply(ctx);
        let saved: Saved = storage.and_then(|s| eframe::get_value(s, STATE_KEY)).unwrap_or_default();
        let mut slots = modules::all();
        for slot in &mut slots {
            if let Some((_, v, o)) = saved.modules.iter().find(|m| m.0 == slot.module.title()) {
                (slot.visible, slot.open) = (*v, *o);
            }
        }
        Self { input: Input::open(saved.device_id.as_deref()), devices: Input::devices(), slots, level: 0.0 }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Input");
            let mut picked = None;
            egui::ComboBox::from_id_salt("input_device")
                .width(240.0)
                .selected_text(&self.input.device_name)
                .show_ui(ui, |ui| {
                    for d in &self.devices {
                        let on = self.input.device_id.as_deref() == Some(d.id.as_str());
                        if ui.selectable_label(on, &d.name).clicked() {
                            picked = Some(d.id.clone());
                        }
                    }
                });
            if ui.button("⟳").on_hover_text("Rescan devices").clicked() {
                self.devices = Input::devices();
            }
            if let Some(id) = picked {
                self.input.switch(Some(&id));
            }
            ui.separator();
            for slot in &mut self.slots {
                ui.toggle_value(&mut slot.visible, slot.module.title());
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let peak = self.input.take_peak();
            self.level = peak.max(self.level * 0.9);
            ui.add(egui::ProgressBar::new(self.level).desired_width(120.0).desired_height(10.0));
            let db = 20.0 * self.level.max(1e-5).log10();
            ui.monospace(format!("{db:>6.1} dBFS"));
            ui.separator();
            ui.label(format!("{} · {} Hz · {} ch", self.input.device_name, self.input.rate, self.input.channels));
            if let Some(e) = self.input.error() {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let n = (self.input.rate as f32 * FRAME_SECS) as usize;
        let mut shared = Shared { frame: self.input.frame(n), gate: Gate::default() };
        // Analysis runs in slot order, so the scope sets the gate first.
        for slot in self.slots.iter_mut().filter(|s| s.visible) {
            slot.module.update(&mut shared);
        }

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for slot in self.slots.iter_mut().filter(|s| s.visible) {
                    ui.push_id(slot.module.title(), |ui| {
                        ui::module_row(ui, slot.module.as_mut(), &mut slot.open, &shared);
                    });
                }
            });
        });
        ui.ctx().request_repaint();
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let saved = Saved {
            device_id: self.input.device_id.clone(),
            modules: self.slots.iter().map(|s| (s.module.title().to_owned(), s.visible, s.open)).collect(),
        };
        eframe::set_value(storage, STATE_KEY, &saved);
    }
}

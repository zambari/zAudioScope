//! Lifted from KitPool: the app title button in the top-left corner and its
//! About popup: build details, the audio inputs the system offers (the one
//! in use marked "(using)"), and Reset state.

const POPUP_W: f32 = 460.0;
const PAD: i8 = 28;
/// The device box's text: secondary detail, kept below the version.
const SMALL_TEXT: f32 = 11.0;

/// What the About popup asks the app to do.
pub enum AboutIntent {
    /// Forget the saved state and restart as on first run.
    ResetState,
}

pub fn show(ui: &mut egui::Ui, devices: &[crate::audio::DeviceInfo], using: Option<&str>) -> Option<AboutIntent> {
    let mut out = None;
    let about = ui
        .add(egui::Button::new(egui::RichText::new("zAUDIOSCOPE").size(13.0).strong()).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("About zAudioScope");
    let confirm_id = about.id.with("reset_confirm");
    let shown = egui::Popup::menu(&about)
        .width(POPUP_W)
        .frame(egui::Frame::popup(ui.style()).inner_margin(PAD))
        // The reset confirmation is a second click inside the popup.
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            // Enforce the content width: Popup::width only supplies an
            // initial size, and the popup can otherwise shrink to fit.
            ui.set_width(POPUP_W - 2.0 * PAD as f32);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.label(egui::RichText::new("zAudioScope").strong());
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.label(format!("Build {}", env!("ZAUDIOSCOPE_BUILD")));
            ui.label(format!("Built {}", env!("ZAUDIOSCOPE_BUILD_DATE")));
            ui.add_space(8.0);
            inputs(ui, devices, using);
            ui.add_space(4.0);
            if reset_state(ui, confirm_id) {
                out = Some(AboutIntent::ResetState);
            }
        });
    if shown.is_none() {
        // Closed: the next open starts on the plain button again.
        ui.data_mut(|d| d.remove::<bool>(confirm_id));
    }
    out
}

/// Audio inputs, small, in a darker inset box so the version above stays
/// the popup's main content.
fn inputs(ui: &mut egui::Ui, devices: &[crate::audio::DeviceInfo], using: Option<&str>) {
    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(4)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(egui::RichText::new("Audio inputs").size(SMALL_TEXT).strong());
            ui.add_space(2.0);
            if devices.is_empty() {
                ui.label(egui::RichText::new("none").size(SMALL_TEXT).weak());
            }
            for d in devices {
                let text = if Some(d.id.as_str()) == using {
                    egui::RichText::new(format!("{} (using)", d.name)).color(super::theme::ACCENT_BRIGHT)
                } else {
                    egui::RichText::new(&d.name)
                };
                ui.label(text.size(SMALL_TEXT)).on_hover_text(&d.id);
            }
        });
}

/// Reset state, then a confirmation. True once confirmed.
fn reset_state(ui: &mut egui::Ui, confirm_id: egui::Id) -> bool {
    let confirming = ui.data(|d| d.get_temp::<bool>(confirm_id)).unwrap_or(false);
    if !confirming {
        if ui
            .button("Reset state…")
            .on_hover_text("Forget the input device, gain and module layout, then restart as on first run.")
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp(confirm_id, true));
        }
        return false;
    }
    ui.label("Restart with a fresh state? Input choice, gain and module layout are forgotten.");
    let mut go = false;
    ui.horizontal(|ui| {
        if ui.button("Reset and restart").clicked() {
            go = true;
        }
        if ui.button("Cancel").clicked() {
            ui.data_mut(|d| d.insert_temp(confirm_id, false));
        }
    });
    go
}

//! Top-bar monitoring controls: on/off, then (while on) the output device,
//! the monitor level, the feedback guard and its state.

use egui::{RichText, Ui};

use super::{meter, theme, widgets};
use crate::audio::monitor::{Trip, risky_pair};
use crate::audio::{DeviceInfo, Monitor};

pub fn show(
    ui: &mut Ui,
    monitor: &mut Monitor,
    meter: &mut meter::Meter,
    devices: &[DeviceInfo],
    input_name: &str,
) {
    let risky = risky_pair(input_name, &monitor.device_name);
    monitor.set_sensitive(risky);
    let warn = ui.visuals().warn_fg_color;

    let mut on = monitor.enabled();
    let mut label = RichText::new("🎧 Monitor");
    if on && risky {
        label = label.color(warn);
    }
    let mut hover =
        "Play the input out of an output device, e.g. headphones, to check what is coming in."
            .to_owned();
    if risky {
        hover += "\n\n⚠ This looks like a built-in microphone into its own speakers: feedback is likely. \
                  Use headphones. The guard is extra sensitive with this pair.";
    }
    if ui
        .toggle_value(&mut on, label)
        .on_hover_text(hover)
        .changed()
    {
        monitor.set_enabled(on);
    }
    if !monitor.enabled() {
        return;
    }

    let (r, picked) = widgets::device_picker(
        ui,
        "monitor_device",
        150.0,
        &monitor.device_name,
        monitor.device_id.as_deref(),
        devices,
    );
    r.on_hover_text("Monitor output device");
    if let Some(id) = picked {
        monitor.select_device(&id);
    }

    let mut gain = monitor.gain();
    let r = meter::gain_slider(ui, &mut gain, |db| {
        format!("Monitor level {db}. Tick marks 0 dB; double-click to reset to it")
    });
    if r.changed() {
        monitor.set_gain(gain);
    }
    meter.levels(ui, monitor.take_peaks(), "Monitor output level");

    let mut guard = monitor.guard();
    if ui
        .toggle_value(&mut guard, "Guard")
        .on_hover_text(
            "Feedback guard: mutes monitoring when the input keeps getting louder on its own as a near-pure \
             tone (the output coming back in through a mic), or a tone sits at clipping. The output is also \
             soft-limited to 0 dBFS.",
        )
        .changed()
    {
        monitor.set_guard(guard);
    }

    if let Some(trip) = monitor.tripped() {
        let why = match trip {
            Trip::Runaway => "the input kept rising on its own",
            Trip::Howl => "a tone sat at clipping",
        };
        let btn = egui::Button::new(RichText::new("⚠ Feedback: muted").color(theme::ON_ACCENT))
            .fill(ui.visuals().error_fg_color);
        if ui
            .add(btn)
            .on_hover_text(format!(
                "The guard muted monitoring: {why}. Click to unmute 6 dB lower; move away from the speakers or \
                 use headphones."
            ))
            .clicked()
        {
            monitor.set_gain(monitor.gain() * 0.5);
            monitor.rearm();
        }
    }
    if let Some(e) = monitor.error() {
        ui.colored_label(ui.visuals().error_fg_color, "⚠ output")
            .on_hover_text(e);
    }
}

//! Window layout: input toolbar, scrollable inspector, proportional display
//! stack, and a status bar.

use serde::{Deserialize, Serialize};

use crate::audio::endpoints;
use crate::audio::mic_permission::{self, MicAuth};
use crate::audio::{
    ChannelSel, DeviceInfo, HISTORY_CHOICES, Input, InputSettings, Monitor, MonitorSettings,
};
use crate::midi::MidiHub;
use crate::modules::{self, FftPlans, Shared, Slot};
use crate::relaunch::BuildWatch;
use crate::ui::dynamic_range::DbRange;
use crate::ui::noterange::NoteRange;
use crate::ui::{self, theme};

const STATE_KEY: &str = "zaudioscope_state";

/// What survives a restart.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    device_id: Option<String>,
    /// Sample rate, channel and pause; `device_id` stays separate so older
    /// saved states still load.
    input: InputSettings,
    /// Per module title: (visible, open).
    modules: Vec<(String, bool, bool)>,
    /// Shared note range of the frequency-axis analysers.
    range: NoteRange,
    /// dB range shared by the locked level controls.
    db: DbRange,
    layout: ui::layout::Workspace,
    debug: bool,
    /// MIDI input port, reopened whenever it is present.
    midi_port: Option<String>,
    /// Monitor output: device, level, feedback guard. Never on at startup.
    monitor: MonitorSettings,
}

pub struct App {
    input: Input,
    /// Plays the input out of an output device.
    monitor: Monitor,
    out_devices: Vec<DeviceInfo>,
    /// Always open, whichever MIDI analysers are shown.
    midi: MidiHub,
    devices: Vec<DeviceInfo>,
    slots: Vec<Slot>,
    gain: f32,
    range: NoteRange,
    db: DbRange,
    layout: ui::layout::Workspace,
    profiler: ui::profiler::Profiler,
    meter: ui::meter::Meter,
    out_meter: ui::meter::Meter,
    /// Levels of every channel, shown in the channel picker.
    ch_meters: ui::meter::ChannelMeters,
    build_watch: BuildWatch,
    /// Start the app again once this one has closed.
    relaunch: bool,
    /// Save a fresh state on the way out (About → Reset state).
    reset_state: bool,
    /// macOS microphone permission; without it input is silent, not an error.
    mic: MicAuth,
    mic_checked: std::time::Instant,
    /// The app macOS charges mic access to (the terminal, when run from one).
    mic_owner: Option<String>,
    /// End of the audio the modules saw last frame, to spot stream restarts.
    last_end: u64,
    /// Per slot: a stream restart happened while the module was not updating,
    /// so its next update must be told about it.
    missed_reset: Vec<bool>,
    levels: crate::audio::levels::LevelHistogram,
    /// FFT plans reused across frames; results are cached per frame only.
    fft_plans: FftPlans,
    /// Endpoint change count the device lists were last refreshed at.
    devices_gen: u64,
}

impl App {
    pub fn new(ctx: &egui::Context, storage: Option<&dyn eframe::Storage>) -> Self {
        ui::theme::apply(ctx);
        let saved: Saved = storage
            .and_then(|s| eframe::get_value(s, STATE_KEY))
            .unwrap_or_default();
        let mut slots = modules::all();
        let slots_len = slots.len();
        for slot in &mut slots {
            if let Some((_, v, o)) = saved.modules.iter().find(|m| m.0 == slot.module.title()) {
                (slot.visible, slot.open) = (*v, *o);
            }
            if let Some(s) = storage {
                let key = slot.params_key();
                slot.module.load_params(s, &key);
            }
        }
        let input = Input::open(&InputSettings {
            device_id: saved.device_id.clone(),
            ..saved.input.clone()
        });
        Self {
            monitor: Monitor::new(&saved.monitor, input.monitor_tap()),
            out_devices: Monitor::devices(),
            input,
            devices: Input::devices(),
            midi: MidiHub::new(
                saved
                    .midi_port
                    .clone()
                    .or_else(|| legacy_midi_port(storage?)),
            ),
            slots,
            // Never persisted: a drag to zero must not survive a restart.
            gain: 1.0,
            range: saved.range.sanitized(),
            db: saved.db.sanitized(),
            layout: saved.layout,
            profiler: ui::profiler::Profiler::new(saved.debug),
            meter: Default::default(),
            out_meter: Default::default(),
            ch_meters: Default::default(),
            build_watch: BuildWatch::new(),
            relaunch: false,
            reset_state: false,
            mic: mic_permission::status(),
            mic_checked: std::time::Instant::now(),
            mic_owner: mic_permission::responsible_app(),
            last_end: 0,
            missed_reset: vec![false; slots_len],
            levels: Default::default(),
            fft_plans: FftPlans::default(),
            devices_gen: endpoints::generation(),
        }
    }

    /// Keep the streams alive across device changes, and the device lists
    /// current. ASIO entries are carried over, not rescanned: listing ASIO
    /// loads its drivers, which the ⟳ button alone may do.
    fn watch_devices(&mut self) {
        self.input.tick();
        self.monitor.tick();
        let generation = endpoints::generation();
        if generation != self.devices_gen {
            self.devices_gen = generation;
            self.devices = Input::devices_refresh(&self.devices);
            self.out_devices = Monitor::devices();
        }
    }

    /// Re-read the permission once a second; reopen the stream once granted.
    fn poll_mic(&mut self, ctx: &egui::Context) {
        if self.mic == MicAuth::Authorized {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        if self.mic_checked.elapsed().as_secs_f32() < 1.0 {
            return;
        }
        self.mic_checked = std::time::Instant::now();
        let now = mic_permission::status();
        if now != self.mic {
            self.mic = now;
            if now == MicAuth::Authorized && !self.input.paused() {
                let id = self.input.device_id.clone();
                self.input.switch(id.as_deref());
            }
        }
    }

    fn mic_warning(&mut self, ui: &mut egui::Ui) {
        let owner = self.mic_owner.as_deref().unwrap_or("this app");
        let err = ui.visuals().error_fg_color;
        match self.mic {
            MicAuth::Authorized => return,
            MicAuth::NotDetermined => {
                ui.colored_label(
                    err,
                    format!("No microphone permission yet for {owner}: input is silent."),
                );
                if ui.button("Ask for access").clicked() {
                    mic_permission::request();
                }
            }
            MicAuth::Denied | MicAuth::Restricted => {
                ui.colored_label(
                    err,
                    format!("Microphone access is off for {owner}: macOS delivers silence instead of audio."),
                )
                .on_hover_text(format!(
                    "macOS grants audio input per app. Started from a shell, permission belongs to the app that \
                     launched it ({owner}). Turn it on under Privacy & Security → Microphone, then quit and \
                     reopen {owner}."
                ));
                if ui.button("Open Microphone settings").clicked() {
                    mic_permission::open_settings();
                }
            }
        }
        ui.separator();
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(ui::infobox::AboutIntent::ResetState) =
                ui::infobox::show(ui, &self.devices, self.input.opened_id())
            {
                self.reset_state = true;
                self.relaunch = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.listen_toggle(ui);
            if self.build_watch.ready() {
                ui.separator();
                let btn = egui::Button::new(
                    egui::RichText::new("New build: relaunch").color(theme::ON_ACCENT),
                )
                .fill(theme::ACCENT);
                if ui
                    .add(btn)
                    .on_hover_text("A newer build is on disk. Quit and start it.")
                    .clicked()
                {
                    self.relaunch = true;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            ui.separator();
            ui.label("Input");
            let (_, picked) = ui::widgets::device_picker(
                ui,
                "input_device",
                240.0,
                &self.input.device_name,
                self.input.device_id.as_deref(),
                &self.devices,
            );
            if ui
                .button("⟳")
                .on_hover_text("Rescan input and output devices")
                .clicked()
            {
                self.devices = Input::devices();
                self.out_devices = Monitor::devices();
            }
            if let Some(id) = picked {
                self.input.select_device(&id);
            }
            self.input_controls(ui);
            ui.separator();
            self.meter.show(ui, &self.input, &mut self.gain);
            ui.separator();
            self.meter.phase_strip(ui, &self.input);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(ui::widgets::flicktoggle(&mut self.profiler.enabled))
                    .on_hover_text("Show UI-thread load and per-module timings");
                ui.label("Debug");
            });
        });
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            self.midi.picker(ui);
            for slot in self.slots.iter_mut().filter(|s| s.module.has_top_row()) {
                ui.separator();
                ui.push_id(slot.module.title(), |ui| slot.module.top_row(ui));
            }
            ui::widgets::right_aligned(ui, "monitor_bar", |ui| {
                ui::monitor_bar::show(
                    ui,
                    &mut self.monitor,
                    &mut self.out_meter,
                    &self.out_devices,
                    &self.input.device_name,
                )
            });
        });
    }

    /// Listening shows an orange frame; clicking releases the interface and
    /// shows an accent-filled "Paused" that invites a click to resume.
    fn listen_toggle(&mut self, ui: &mut egui::Ui) {
        let mut listening = !self.input.paused();
        let before = listening;
        ui.horizontal(|ui| {
            // Icon drawn as a shape (see theme::transport); clicking it toggles too.
            let (rect, icon) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::click());
            if icon.clicked() {
                listening = !listening;
            }
            if ui.is_rect_visible(rect) {
                let color = if listening {
                    ui.visuals().text_color()
                } else {
                    theme::ACCENT_BRIGHT
                };
                crate::ui::icons::transport(ui.painter(), rect.center(), listening, color);
            }
            // Listening: quiet orange frame. Paused: solid accent fill, so the
            // button that resumes is the loudest thing in the bar.
            let button = if listening {
                egui::Button::new("Listening")
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.8)))
            } else {
                egui::Button::new(egui::RichText::new("Paused").color(theme::ON_ACCENT))
                    .fill(theme::ACCENT)
            };
            let hint = if listening {
                "Click to pause: disconnects from the interface so another app (e.g. a DAW in exclusive mode) can use it. The last captured audio stays on screen."
            } else {
                "Click to resume listening."
            };
            if ui.add(button).on_hover_text(hint).clicked() {
                listening = !listening;
            }
        });
        if listening != before {
            self.input.set_paused(!listening);
        }
    }

    /// The channel picker: a popup with every channel's level, the sample
    /// rate and the buffer size.
    fn input_controls(&mut self, ui: &mut egui::Ui) {
        let n = self.input.channels;
        let cur = self.input.channel();
        let head = format!(
            "{}{} · {}",
            cur.label(),
            if cur.fits(n) { "" } else { " (n/a)" },
            short_rate(self.input.rate)
        );
        let mut pick_ch = None;
        let mut pick_rate = None;
        let mut pick_buf = None;
        let mut pick_hist = None;
        let popup = egui::ComboBox::from_id_salt("input_channel")
            .width(130.0)
            .height(ui::widgets::POPUP_HEIGHT)
            .selected_text(head)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                ui.set_min_width(300.0);
                let dt = ui.input(|i| i.stable_dt).min(0.1);
                self.ch_meters.update(&self.input.take_all_peaks(), dt);
                ui.ctx().request_repaint();

                egui::Grid::new("input_setup").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    ui.label("Rate").on_hover_text("Sample rate (reopens the device; on macOS this sets the interface's rate)");
                    ui.horizontal_wrapped(|ui| {
                        let want = self.input.settings().rate;
                        if ui
                            .selectable_label(want.is_none(), "Auto")
                            .on_hover_text(format!("The device's own rate ({} Hz)", self.input.default_rate))
                            .clicked()
                        {
                            pick_rate = Some(None);
                        }
                        for &r in &self.input.rates {
                            if ui.selectable_label(want == Some(r), short_rate(r)).clicked() {
                                pick_rate = Some(Some(r));
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("Buffer").on_hover_text(
                        "Frames per audio callback (reopens the device). Smaller reacts sooner, larger is safer against dropouts.",
                    );
                    ui.horizontal(|ui| {
                        let want = self.input.buffer();
                        let head = want.map_or_else(|| "Auto".to_string(), |b| b.to_string());
                        let mut sel = want;
                        ui.add_enabled_ui(!self.input.buffer_sizes.is_empty(), |ui| {
                            egui::ComboBox::from_id_salt("input_buffer")
                                .width(90.0)
                                .selected_text(head)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sel, None, "Auto")
                                        .on_hover_text("The driver's default");
                                    for &b in &self.input.buffer_sizes {
                                        ui.selectable_value(&mut sel, Some(b), b.to_string());
                                    }
                                });
                        });
                        if sel != want {
                            pick_buf = Some(sel);
                        }
                        if self.input.buffer_sizes.is_empty() {
                            ui.weak("not adjustable on this device");
                        }
                    });
                    ui.end_row();

                    ui.label("Capture").on_hover_text(
                        "Seconds of audio kept, accumulated from however small the callbacks are. Changing it restarts the capture.",
                    );
                    ui.horizontal_wrapped(|ui| {
                        let want = self.input.history_secs();
                        for s in HISTORY_CHOICES {
                            if ui.selectable_label(want == s, format!("{s}s")).clicked() {
                                pick_hist = Some(s);
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("");
                    let f = self.input.callback_frames();
                    if f > 0 {
                        ui.weak(format!(
                            "running {f} frames · {:.1} ms",
                            f as f32 * 1000.0 / self.input.rate as f32
                        ));
                    } else {
                        ui.weak("not running");
                    }
                    ui.end_row();
                });
                ui.separator();

                // One row per pair: both channels with their levels, then
                // the pair itself.
                let bar = egui::vec2(110.0, 6.0);
                let ch_btn = |ui: &mut egui::Ui, sel: ChannelSel, meters: &ui::meter::ChannelMeters, k: usize| {
                    let clicked = ui
                        .add_sized([44.0, 18.0], egui::Button::selectable(cur == sel, sel.label()))
                        .clicked();
                    meters.bar(ui, k, bar);
                    clicked
                };
                egui::Grid::new("input_channels").num_columns(3).spacing([10.0, 3.0]).show(ui, |ui| {
                    for c in (0..n).step_by(2) {
                        ui.horizontal(|ui| {
                            if ch_btn(ui, ChannelSel::Mono(c), &self.ch_meters, c as usize) {
                                pick_ch = Some(ChannelSel::Mono(c));
                            }
                        });
                        if c + 1 < n {
                            ui.horizontal(|ui| {
                                if ch_btn(ui, ChannelSel::Mono(c + 1), &self.ch_meters, c as usize + 1) {
                                    pick_ch = Some(ChannelSel::Mono(c + 1));
                                }
                            });
                            let sel = ChannelSel::Pair(c);
                            if ui
                                .add_sized([60.0, 18.0], egui::Button::selectable(cur == sel, sel.label()))
                                .on_hover_text("L/R for the meter and phase strip, averaged for the analysers")
                                .clicked()
                            {
                                pick_ch = Some(sel);
                            }
                        }
                        ui.end_row();
                    }
                });
                if n == 0 {
                    ui.weak("No channels: device not open");
                }
            });
        // Measure every channel only while the picker is open.
        self.input.set_meter_all(popup.inner.is_some());
        popup
            .response
            .on_hover_text("Input channel or pair, sample rate and buffer size");

        if let Some(sel) = pick_ch {
            self.input.set_channel(sel);
        }
        if let Some(r) = pick_rate {
            self.input.set_rate(r);
        }
        if let Some(b) = pick_buf {
            self.input.set_buffer(b);
        }
        if let Some(h) = pick_hist {
            self.input.set_history_secs(h);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            self.mic_warning(ui);
            let ch = self.input.channel();
            let ch = if ch.fits(self.input.channels) { ch.label() } else { format!("{} not on device, using Ch 1/2", ch.label()) };
            ui.label(format!(
                "{} · {} Hz · {} of {} ch",
                self.input.device_name, self.input.rate, ch, self.input.channels
            ));
            if self.input.paused() {
                ui.separator();
                ui.colored_label(theme::ACCENT, "paused: interface released");
            } else if self.input.suspended() {
                ui.separator();
                ui.weak("paused: app in background");
            }
            let (cap, filled) = self.input.captured();
            if filled < cap && !self.input.paused() {
                ui.separator();
                let r = self.input.rate.max(1) as f32;
                ui.weak(format!("capturing {:.1} / {:.1} s", filled as f32 / r, cap as f32 / r));
            }
            let x = self.input.xruns();
            if x > 0 {
                ui.separator();
                ui.label(format!("{x} dropout{}", if x == 1 { "" } else { "s" }))
                    .on_hover_text("Buffer under/overruns reported by the driver since the stream opened. A few at start or on a rate change are normal; a steady count means the system is overloaded.");
            }
            let z = self.input.skipped();
            if z > 0 {
                ui.separator();
                let ms = z as f32 * 1000.0 / self.input.rate.max(1) as f32;
                ui.label(format!("{ms:.0} ms of empty input skipped"))
                    .on_hover_text("Blocks of exact zeros the driver delivered right after a stall or dropout: filler for audio that never arrived, left out so the analysers don't see false silence. Real silence on a healthy stream is kept.");
            }
            let g = self.input.gaps();
            if g > 0 {
                ui.separator();
                ui.colored_label(ui.visuals().warn_fg_color, format!("input interrupted {g}×"))
                    .on_hover_text("Audio stopped arriving for longer than a buffer. If this keeps counting up, macOS is pausing the input: usually missing microphone permission for the app that launched this one, or another app holding the interface exclusively. The menu-bar mic indicator blinks when this happens.");
            }
            if self.input.fallback() {
                let wanted = self.input.device_id.as_deref();
                let name = self
                    .devices
                    .iter()
                    .find(|d| Some(d.id.as_str()) == wanted)
                    .map_or("Saved input", |d| d.name.as_str());
                ui.separator();
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("{name} missing: using {}", self.input.device_name),
                )
                .on_hover_text("Switches back by itself once the device is connected again.");
            }
            if let Some(e) = self.input.error() {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let timing = self.profiler.begin();
        self.build_watch.poll(ui.ctx());
        self.poll_mic(ui.ctx());
        // Release the interface while another app has focus; resume on return.
        let focused = ui.input(|i| i.focused);
        self.input.set_suspended(!focused);
        self.watch_devices();
        // Live modules: visible, and drawn by the page on screen (a solo page
        // draws one analyser), or needed by others. The rest pause in place.
        let midi = self.midi.connected();
        for s in &mut self.slots {
            s.available = midi || !s.module.needs_midi();
        }
        let requested = self.layout.requested(&self.slots);
        let live: Vec<bool> = self
            .slots
            .iter()
            .zip(&requested)
            .map(|(s, &want)| s.shown() && (want || s.module.always_live()))
            .collect();
        // Snapshot only as much as the live modules need; the ring itself
        // may hold far more (the capture length).
        let rate = self.input.rate;
        let n = self
            .slots
            .iter()
            .zip(&live)
            .filter(|(_, l)| **l)
            .map(|(s, _)| s.module.lookback(rate))
            .max()
            .unwrap_or(0);
        // Analysis is clocked by the audio, not the repaint: modules look at
        // how far the stream has moved since last frame and do nothing when
        // no new audio arrived.
        let frame = self.input.frame(n);
        let reset = frame.end() < self.last_end;
        self.last_end = frame.end();
        let start = self.profiler.start();
        self.midi.poll();
        self.profiler.record("Ingest", "MIDI", start);
        let start = self.profiler.start();
        self.levels.ingest(&frame, reset);
        self.profiler.record("Ingest", "Level histo", start);
        let mut shared = Shared::new(
            frame,
            reset,
            self.range,
            self.db,
            self.levels.dist(),
            std::mem::take(&mut self.fft_plans),
        );
        shared.midi = self.midi.frame();
        // How long the input has been quiet, from last frame's meter.
        shared.quiet_s = self.meter.quiet_s();
        shared.connected = self.input.connected();
        // Analysis runs in slot order, so the scope sets the gate first.
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if !live[i] {
                // Paused: remember a restart so it is applied on resume.
                self.missed_reset[i] |= reset;
                continue;
            }
            let start = self.profiler.start();
            shared.reset = reset || std::mem::take(&mut self.missed_reset[i]);
            slot.module.update(&mut shared);
            self.profiler.record("Analysis", slot.module.title(), start);
        }

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        let mut marks = Vec::new();
        for (slot, _) in self.slots.iter().zip(&live).filter(|(_, l)| **l) {
            slot.module.marks(&mut marks);
        }
        let debug_anchor = ui.available_rect_before_wrap().right_top() + egui::vec2(-6.0, 6.0);
        // Paused or no audio arriving: fade the scopes well back so the
        // stale picture doesn't read as live.
        let idle = !self.input.connected();
        egui::CentralPanel::default().show(ui, |ui| {
            if idle {
                ui.multiply_opacity(0.25);
            }
            self.layout
                .show(ui, &mut self.slots, &shared, &marks, &mut self.profiler);
        });
        self.range = shared.range.get().sanitized();
        self.db = shared.db.get().sanitized();
        // Frame done: drop the cached spectra, keep the plans.
        self.fft_plans = shared.finish();
        if self.profiler.enabled {
            self.profiler.note = self.fft_plans.report();
        }
        self.profiler.finish(ui, timing, debug_anchor);
        ui.ctx().request_repaint();
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let saved = if self.reset_state {
            Saved::default()
        } else {
            Saved {
                device_id: self.input.device_id.clone(),
                input: self.input.settings(),
                modules: self
                    .slots
                    .iter()
                    .map(|s| (s.module.title().to_owned(), s.visible, s.open))
                    .collect(),
                range: self.range,
                db: self.db,
                layout: self.layout.clone(),
                debug: self.profiler.enabled,
                midi_port: self.midi.port().cloned(),
                monitor: self.monitor.settings(),
            }
        };
        eframe::set_value(storage, STATE_KEY, &saved);
        // On reset, overwrite with fresh modules' defaults.
        let fresh;
        let slots = if self.reset_state {
            fresh = modules::all();
            &fresh
        } else {
            &self.slots
        };
        for slot in slots {
            slot.module.save_params(storage, &slot.params_key());
        }
    }

    fn on_exit(&mut self) {
        if !self.relaunch {
            return;
        }
        // The dev build on disk, else (Reset state in a release build) the
        // running binary.
        let exe = self
            .build_watch
            .exe()
            .cloned()
            .or_else(|| std::env::current_exe().ok());
        if let Some(exe) = exe
            && let Err(e) = std::process::Command::new(exe).spawn()
        {
            eprintln!("relaunch failed: {e}");
        }
    }
}

/// The port the MIDI module chose before the app owned the MIDI input.
fn legacy_midi_port(storage: &dyn eframe::Storage) -> Option<String> {
    #[derive(Deserialize)]
    struct Old {
        port: Option<String>,
    }
    eframe::get_value::<Old>(storage, "zaudioscope_module.MIDI")?.port
}

/// "48k", "44.1k".
fn short_rate(r: u32) -> String {
    if r.is_multiple_of(1000) {
        format!("{}k", r / 1000)
    } else {
        format!("{:.1}k", r as f32 / 1000.0)
    }
}

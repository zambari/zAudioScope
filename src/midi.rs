//! The MIDI input, owned by the app and alive whether or not any MIDI
//! analyser is shown. A background thread rescans the ports once a second;
//! the app polls the hub every frame, which drops the connection when its
//! device vanishes, reopens the chosen port once it is back, and drains the
//! messages received since the last frame. Analysers read those through
//! [`MidiFrame`] in `Shared`.
//!
//! The midir callback runs on its own thread; it timestamps each channel
//! message with [`now`] and pushes it onto a shared queue.

use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui::Ui;

use crate::ui::theme;

/// How often the port list is rescanned to notice devices coming and going.
const WATCHDOG: Duration = Duration::from_secs(1);

/// Seconds since the process' MIDI clock started; what [`Event::t`] counts.
pub fn now() -> f64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// A channel message as received.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Seconds on the [`now`] clock.
    pub t: f64,
    /// Status, data 1, data 2 (0 when absent).
    pub b: [u8; 3],
}

/// What the analysers see each frame.
#[derive(Clone, Default)]
pub struct MidiFrame {
    /// Messages received since the last frame, oldest first.
    pub events: Rc<[Event]>,
    /// The connection closed this frame (unplugged, deselected, switched):
    /// held notes should end.
    pub dropped: bool,
    pub connected: bool,
    /// A port is chosen but not open (unplugged).
    pub missing: bool,
}

fn input(name: &str) -> Result<midir::MidiInput, String> {
    let mut i = midir::MidiInput::new(name).map_err(|e| e.to_string())?;
    // Channel messages only: no SysEx, clock or active sensing.
    i.ignore(midir::Ignore::All);
    Ok(i)
}

fn list_ports() -> Vec<String> {
    // A fresh client each time: a long-lived one may not see ports come and go.
    match input("zAudioScope scan") {
        Ok(i) => i
            .ports()
            .iter()
            .filter_map(|p| i.port_name(p).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Rescans in the background so the UI thread never waits on CoreMIDI and
/// the list stays current even when nothing asks for it.
fn spawn_scanner() -> Arc<Mutex<Option<Vec<String>>>> {
    let slot = Arc::new(Mutex::new(None));
    let out = slot.clone();
    let spawned = std::thread::Builder::new()
        .name("midi watchdog".into())
        .spawn(move || {
            loop {
                let ports = list_ports();
                match out.lock() {
                    Ok(mut s) => *s = Some(ports),
                    Err(_) => return,
                }
                // Stop once the hub is gone.
                if Arc::strong_count(&out) == 1 {
                    return;
                }
                std::thread::sleep(WATCHDOG);
            }
        });
    if spawned.is_err() {
        // No thread: scan once here so the list is at least filled.
        *slot.lock().unwrap() = Some(list_ports());
    }
    slot
}

pub struct MidiHub {
    /// Chosen port name, reconnected whenever present. Persisted by the app.
    port: Option<String>,
    ports: Vec<String>,
    scanned: Arc<Mutex<Option<Vec<String>>>>,
    conn: Option<midir::MidiInputConnection<()>>,
    /// Port the connection is open on.
    connected: Option<String>,
    error: Option<String>,
    queue: Arc<Mutex<Vec<Event>>>,
    frame: MidiFrame,
    /// Set by `disconnect`, reported in the next frame.
    dropped: bool,
    /// The process' first CoreMIDI client, made on the UI thread: CoreMIDI
    /// refreshes its device list through the run loop of the thread that
    /// created the first client, and only the UI thread runs one. Made on
    /// the watchdog thread instead, the list would never change.
    _anchor: Option<midir::MidiInput>,
}

impl MidiHub {
    pub fn new(port: Option<String>) -> Self {
        now(); // start the clock
        let _anchor = input("zAudioScope").ok(); // before the watchdog's first scan
        Self {
            port,
            ports: Vec::new(),
            scanned: spawn_scanner(),
            conn: None,
            connected: None,
            error: None,
            queue: Default::default(),
            frame: MidiFrame::default(),
            dropped: false,
            _anchor,
        }
    }

    pub fn port(&self) -> Option<&String> {
        self.port.as_ref()
    }

    /// This frame's messages and connection state.
    pub fn frame(&self) -> MidiFrame {
        self.frame.clone()
    }

    fn missing(&self) -> bool {
        self.port.is_some() && self.connected.is_none()
    }

    fn disconnect(&mut self) {
        if self.connected.is_some() {
            self.dropped = true;
        }
        self.conn = None;
        self.connected = None;
    }

    fn connect(&mut self, name: &str) {
        self.disconnect();
        self.error = None;
        let result = (|| {
            let i = input("zAudioScope")?;
            let port = i
                .ports()
                .into_iter()
                .find(|p| i.port_name(p).ok().as_deref() == Some(name))
                .ok_or_else(|| format!("{name}: not found"))?;
            let queue = self.queue.clone();
            i.connect(
                &port,
                "zAudioScope in",
                move |_, msg, _| {
                    let Some(&status) = msg.first() else { return };
                    if !(0x80..0xF0).contains(&status) {
                        return;
                    }
                    let b = [
                        status,
                        msg.get(1).copied().unwrap_or(0),
                        msg.get(2).copied().unwrap_or(0),
                    ];
                    if let Ok(mut q) = queue.lock() {
                        q.push(Event { t: now(), b });
                    }
                },
                (),
            )
            .map_err(|e| e.to_string())
        })();
        match result {
            Ok(c) => {
                self.conn = Some(c);
                self.connected = Some(name.to_owned());
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Once per frame, before the analysers run: follow the latest port
    /// scan, then collect what arrived.
    pub fn poll(&mut self) {
        let fresh = self.scanned.lock().ok().and_then(|mut s| s.take());
        if let Some(ports) = fresh {
            self.ports = ports;
            if self
                .connected
                .as_ref()
                .is_some_and(|n| !self.ports.contains(n))
            {
                self.disconnect();
                self.error = None;
            }
            if self.connected.is_none()
                && let Some(name) = self.port.clone().filter(|n| self.ports.contains(n))
            {
                self.connect(&name);
            }
        }
        let events = std::mem::take(&mut *self.queue.lock().unwrap_or_else(|e| e.into_inner()));
        self.frame = MidiFrame {
            events: events.into(),
            dropped: std::mem::take(&mut self.dropped),
            connected: self.connected.is_some(),
            missing: self.missing(),
        };
    }

    fn select(&mut self, port: Option<String>) {
        self.port = port.clone();
        match port {
            Some(name) => self.connect(&name),
            None => {
                self.disconnect();
                self.error = None;
            }
        }
    }

    /// "MIDI in" and the port dropdown, with any connection error.
    pub fn picker(&mut self, ui: &mut Ui) {
        ui.label("MIDI in");
        let missing = self.missing();
        // While the chosen device is away the input is effectively none; the
        // name is kept so it reopens when the device returns.
        let shown = match &self.connected {
            Some(name) => egui::RichText::new(name),
            None => egui::RichText::new("— none —"),
        };
        let combo = egui::ComboBox::from_id_salt("midi_port")
            .width(200.0)
            .selected_text(shown)
            .show_ui(ui, |ui| {
                let mut pick = None;
                if ui
                    .selectable_label(self.connected.is_none(), "— none —")
                    .clicked()
                {
                    pick = Some(None);
                }
                // The chosen port while unplugged: shown, but not pickable.
                if let Some(name) = self.port.as_ref().filter(|_| missing) {
                    ui.add_enabled(
                        false,
                        egui::Button::selectable(false, format!("{name} (waiting for device)")),
                    );
                }
                for name in &self.ports {
                    if ui
                        .selectable_label(self.connected.as_ref() == Some(name), name)
                        .clicked()
                    {
                        pick = Some(Some(name.clone()));
                    }
                }
                pick
            });
        if let Some(name) = self.port.as_ref().filter(|_| missing) {
            combo
                .response
                .on_hover_text(format!("{name} is unplugged; it reopens when it comes back"));
        }
        if let Some(pick) = combo.inner.flatten() {
            self.select(pick);
        }
        if let Some(e) = &self.error {
            ui.colored_label(theme::HANDLE, e);
        }
    }
}

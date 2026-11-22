//! MIDI after-glow on the note range keys. Every key remembers whether it is
//! held (note on without a matching off) and keeps a slowly cooling "heat"
//! that each note on adds to. A held key fades in quickly; on release it fades
//! out over a time proportional to how long it was held, down to a dim glow
//! set by its heat, which cools over tens of seconds. Keys played often stay
//! warm: after a phrase the most-used notes keep glowing for a while.
//!
//! Listens to the app's MIDI input (every channel, or one), raw note numbers.
//! [`Timing`] stretches the release and the heat's cooling.

use crate::midi::MidiFrame;

/// Fade-in time constant after a note on, s.
const ATTACK_S: f32 = 0.035;
/// Release time constant: this plus `RELEASE_PER_HELD` × held time, capped.
const RELEASE_MIN_S: f32 = 0.12;
const RELEASE_PER_HELD: f32 = 0.6;
const RELEASE_MAX_S: f32 = 4.0;
/// Heat added per note on, and how fast it cools (time constant, s).
const HEAT_PER_HIT: f32 = 0.07;
const HEAT_TAU_S: f32 = 20.0;
/// Brightness while held: this plus the heat, capped at 1.
const HELD_BASE: f32 = 0.55;
/// After release, the glow settles at heat × this.
const AFTERGLOW: f32 = 0.6;
/// Below this a key counts as dark.
const DARK: f32 = 0.004;

#[derive(Clone, Copy, Default)]
struct Key {
    /// Note on seen, note off not yet: the "bit".
    held: bool,
    /// When the current / last hold began, on the MIDI clock.
    on_t: f64,
    /// Release time constant fixed at note off, s.
    release_s: f32,
    heat: f32,
    /// What is drawn, 0..1.
    level: f32,
}

/// Adjustable fade times.
#[derive(Clone, Copy)]
pub struct Timing {
    /// Multiplies the release time constant.
    pub release: f32,
    /// Heat cooling time constant, s.
    pub heat_tau_s: f32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            release: 1.0,
            heat_tau_s: HEAT_TAU_S,
        }
    }
}

#[derive(Clone)]
pub struct KeyGlow {
    keys: [Key; 128],
    last_t: Option<f64>,
    pub timing: Timing,
}

impl Default for KeyGlow {
    fn default() -> Self {
        Self {
            keys: [Key::default(); 128],
            last_t: None,
            timing: Timing::default(),
        }
    }
}

impl KeyGlow {
    fn release(k: &mut Key, t: f64, scale: f32) {
        if k.held {
            k.held = false;
            let held = (t - k.on_t).max(0.0) as f32;
            k.release_s = (RELEASE_MIN_S + RELEASE_PER_HELD * held).min(RELEASE_MAX_S) * scale;
        }
    }

    /// Once per frame: take this frame's MIDI (only `channel` 0..16 when
    /// set), then advance the fades. Returns true while anything is still
    /// lit (worth a repaint).
    pub fn update(&mut self, midi: &MidiFrame, channel: Option<u8>) -> bool {
        let Timing { release, heat_tau_s } = self.timing;
        let release = release.max(0.05);
        for e in midi.events.iter() {
            let [status, note, vel] = e.b;
            if channel.is_some_and(|c| c != status & 0x0F) {
                continue;
            }
            match (status & 0xF0, vel) {
                (0x90, v) if v > 0 => {
                    let k = &mut self.keys[(note & 0x7F) as usize];
                    k.held = true;
                    k.on_t = e.t;
                    k.heat = (k.heat + HEAT_PER_HIT).min(1.0);
                }
                (0x80, _) | (0x90, _) => Self::release(&mut self.keys[(note & 0x7F) as usize], e.t, release),
                // All sound off / all notes off.
                (0xB0, _) if note == 120 || note == 123 => {
                    self.keys.iter_mut().for_each(|k| Self::release(k, e.t, release))
                }
                _ => {}
            }
        }
        let now = crate::midi::now();
        if midi.dropped {
            self.keys.iter_mut().for_each(|k| Self::release(k, now, release));
        }
        // Cap the step so a stalled frame doesn't jump.
        let dt = self
            .last_t
            .map_or(0.0, |t| (now - t).clamp(0.0, 0.25) as f32);
        self.last_t = Some(now);
        let cool = (-dt / heat_tau_s.max(0.1)).exp();
        let mut lit = false;
        for k in &mut self.keys {
            k.heat *= cool;
            let target = if k.held {
                (HELD_BASE + k.heat).min(1.0)
            } else {
                k.heat * AFTERGLOW
            };
            let tau = if k.held || target > k.level {
                ATTACK_S
            } else {
                k.release_s.max(RELEASE_MIN_S * release)
            };
            k.level += (target - k.level) * (1.0 - (-dt / tau).exp());
            if k.level < DARK && !k.held {
                k.level = 0.0;
            }
            lit |= k.level > 0.0 || k.held;
        }
        lit
    }

    fn key(&self, key: i32) -> Option<&Key> {
        usize::try_from(key).ok().and_then(|i| self.keys.get(i))
    }

    /// Brightness of MIDI note `key`, 0..1.
    pub fn level(&self, key: i32) -> f32 {
        self.key(key).map_or(0.0, |k| k.level)
    }

    pub fn held(&self, key: i32) -> bool {
        self.key(key).is_some_and(|k| k.held)
    }
}

//! Debounced note readouts: a note must be heard clearly for `confirm`
//! seconds before it is shown, and stays shown for `hold` seconds after it
//! drops out, so the letters don't flicker frame to frame.
//!
//! Readings keep the slot order they were confirmed in: when a held note
//! drops out and a new one starts, the old stays on top (fading over the
//! hold) with the newcomer below it, then the newcomer slides up.

use super::note_input::ease;
use crate::pitch;

/// Readings shown at most.
pub const MAX_SHOWN: usize = 3;
/// A sighting this close (semitones) to a reading belongs to it, even if it
/// would snap to another target: hysteresis between sparse snap targets.
const CAPTURE_SEMIS: f32 = 0.5;
/// While unseen, unconfirmed evidence leaks away with this time constant:
/// a short dropout during an onset doesn't restart the count, scattered
/// blips never add up to a confirmation.
const LEAK_TAU: f32 = 0.08;
/// Alpha a held reading fades down to by the end of its hold.
const HELD_ALPHA: f32 = 0.45;

pub struct Timing {
    /// Seconds of clear sighting before a note is shown.
    pub confirm: f32,
    /// Seconds a note stays shown after it was last seen.
    pub hold: f32,
    /// Time constant of the pitch (cents) smoothing.
    pub glide: f32,
}

pub struct Reading {
    /// Snapped target note.
    pub note: i32,
    /// Smoothed fractional MIDI pitch.
    pub midi: f32,
    pub clarity: f32,
    /// Seen this frame.
    pub live: bool,
    /// Seconds since last seen (0 while live); infinite once evicted.
    pub since: f32,
    pub alpha: f32,
    /// Eased slot position, 0 = top.
    pub slot: f32,
    /// Seconds of sighting, leaky until confirmed.
    evidence: f32,
    /// Confirmation order, `None` until confirmed.
    order: Option<u64>,
}

impl Reading {
    pub fn cents(&self) -> f32 {
        (self.midi - self.note as f32) * 100.0
    }

    /// Fraction of the hold left, 1 while live.
    pub fn hold_left(&self, hold: f32) -> f32 {
        if hold > 0.0 {
            (1.0 - self.since / hold).clamp(0.0, 1.0)
        } else {
            self.live as u8 as f32
        }
    }

    fn shown(&self, hold: f32) -> bool {
        self.order.is_some() && self.since <= hold
    }
}

#[derive(Default)]
pub struct NoteHold {
    /// Notes an octave apart are one reading: a sighting joins a reading of
    /// the same note name and is moved into its octave, so a pitch that
    /// flips between octaves doesn't split or restart.
    pub any_octave: bool,
    readings: Vec<Reading>,
    next_order: u64,
}

impl NoteHold {
    /// Advance by `dt` seconds. `seen` holds (fractional MIDI, clarity) of
    /// every clear candidate this frame, snapped to the nearest of `targets`.
    pub fn step(&mut self, dt: f32, seen: &[(f32, f32)], targets: &[i32], t: &Timing) {
        for r in &mut self.readings {
            r.live = false;
        }
        for &(m, cl) in seen {
            let i = self.find(m, targets, t.hold);
            let fold = self.any_octave;
            let r = &mut self.readings[i];
            let m = if fold {
                m + 12.0 * ((r.midi - m) / 12.0).round()
            } else {
                m
            };
            if r.evidence == 0.0 && r.order.is_none() {
                r.midi = m;
            } else if !r.live {
                r.midi += (m - r.midi) * ease(dt, t.glide);
            }
            r.clarity = if r.live { r.clarity.max(cl) } else { cl };
            r.live = true;
        }

        let mut shown = self.readings.iter().filter(|r| r.shown(t.hold)).count();
        for r in &mut self.readings {
            if r.live {
                r.since = 0.0;
                r.evidence += dt;
                if r.order.is_none() && r.evidence >= t.confirm {
                    r.order = Some(self.next_order);
                    self.next_order += 1;
                    r.slot = shown as f32;
                    shown += 1;
                }
            } else {
                r.since += dt;
                if r.order.is_none() {
                    r.evidence *= 1.0 - ease(dt, LEAK_TAU);
                }
            }
        }
        // Too many: retire the longest-held, else the least clear.
        while shown > MAX_SHOWN {
            let victim = self
                .readings
                .iter_mut()
                .filter(|r| r.shown(t.hold))
                .min_by(|a, b| {
                    (a.live, -a.since, a.clarity)
                        .partial_cmp(&(b.live, -b.since, b.clarity))
                        .unwrap()
                })
                .unwrap();
            victim.since = f32::INFINITY;
            shown -= 1;
        }

        let mut slots: Vec<(u64, usize)> = self
            .readings
            .iter()
            .enumerate()
            .filter(|(_, r)| r.shown(t.hold))
            .map(|(i, r)| (r.order.unwrap(), i))
            .collect();
        slots.sort_unstable();
        for (slot, &(_, i)) in slots.iter().enumerate() {
            let r = &mut self.readings[i];
            r.slot += (slot as f32 - r.slot) * ease(dt, 0.15);
        }
        for r in &mut self.readings {
            let target = if !r.shown(t.hold) {
                0.0
            } else {
                HELD_ALPHA + (1.0 - HELD_ALPHA) * r.hold_left(t.hold).max(r.live as u8 as f32)
            };
            let tau = if target > r.alpha { 0.08 } else { 0.12 };
            r.alpha += (target - r.alpha) * ease(dt, tau);
        }
        self.readings.retain(|r| {
            if r.order.is_some() {
                r.shown(t.hold) || r.alpha > 0.01
            } else {
                r.live || r.evidence > 1e-3
            }
        });
    }

    /// Index of the reading a sighting at `m` belongs to, created if needed.
    fn find(&mut self, m: f32, targets: &[i32], hold: f32) -> usize {
        let open = |r: &Reading| r.order.is_none() || r.since <= hold;
        let fold = self.any_octave;
        let gap = |r: &Reading| {
            if fold {
                pitch::octave_free_gap(r.midi, m)
            } else {
                (r.midi - m).abs()
            }
        };
        let near = self
            .readings
            .iter()
            .enumerate()
            .filter(|(_, r)| open(r) && gap(r) < CAPTURE_SEMIS)
            .min_by(|a, b| gap(a.1).total_cmp(&gap(b.1)))
            .map(|(i, _)| i);
        if let Some(i) = near {
            return i;
        }
        let note = pitch::snap(m, targets.iter().copied()).map_or(m.round() as i32, |s| s.0);
        let same = |n: i32| {
            if fold {
                (n - note).rem_euclid(12) == 0
            } else {
                n == note
            }
        };
        if let Some(i) = self.readings.iter().position(|r| open(r) && same(r.note)) {
            return i;
        }
        self.readings.push(Reading {
            note,
            midi: m,
            clarity: 0.0,
            live: false,
            since: 0.0,
            alpha: 0.0,
            slot: 0.0,
            evidence: 0.0,
            order: None,
        });
        self.readings.len() - 1
    }

    /// Confirmed readings worth drawing, any order.
    pub fn visible(&self) -> impl Iterator<Item = &Reading> {
        self.readings
            .iter()
            .filter(|r| r.order.is_some() && r.alpha > 0.01)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Timing = Timing {
        confirm: 0.15,
        hold: 0.6,
        glide: 0.05,
    };
    const DT: f32 = 1.0 / 60.0;

    fn run(h: &mut NoteHold, secs: f32, seen: &[(f32, f32)]) {
        let targets: Vec<i32> = (0..128).collect();
        for _ in 0..(secs / DT).round() as usize {
            h.step(DT, seen, &targets, &T);
        }
    }

    fn notes(h: &NoteHold) -> Vec<i32> {
        let mut v: Vec<&Reading> = h.visible().filter(|r| r.since <= T.hold).collect();
        v.sort_by(|a, b| a.slot.total_cmp(&b.slot));
        v.iter().map(|r| r.note).collect()
    }

    #[test]
    fn blips_are_ignored() {
        let mut h = NoteHold::default();
        for _ in 0..10 {
            run(&mut h, 0.05, &[(40.0, 0.9)]);
            run(&mut h, 0.1, &[]);
        }
        assert!(notes(&h).is_empty());
    }

    #[test]
    fn holds_then_hands_over() {
        let mut h = NoteHold::default();
        run(&mut h, 0.3, &[(40.1, 0.9)]);
        assert_eq!(notes(&h), [40]);
        // Drops out, a new note starts: old stays on top during its hold.
        run(&mut h, 0.3, &[(45.0, 0.9)]);
        assert_eq!(notes(&h), [40, 45]);
        run(&mut h, 0.5, &[(45.0, 0.9)]);
        assert_eq!(notes(&h), [45]);
    }

    #[test]
    fn any_octave_keeps_one_reading() {
        let mut h = NoteHold {
            any_octave: true,
            ..Default::default()
        };
        // D#3 and D#4 alternating every frame-ish: one D# reading.
        for _ in 0..10 {
            run(&mut h, 0.05, &[(51.1, 0.9)]);
            run(&mut h, 0.05, &[(63.1, 0.9)]);
        }
        let n = notes(&h);
        assert_eq!(n.len(), 1, "{n:?}");
        assert_eq!(n[0].rem_euclid(12), 3);
    }

    #[test]
    fn at_most_three() {
        let mut h = NoteHold::default();
        for m in [40.0, 42.0, 44.0, 46.0] {
            run(&mut h, 0.2, &[(m, 0.9)]);
        }
        assert_eq!(notes(&h), [42, 44, 46]);
    }
}

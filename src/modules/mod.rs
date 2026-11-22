//! Analyser modules. Parameters live in the inspector, graphs in a resizable
//! display stack. The scope runs first and decides which slice of audio the
//! others analyse (the gate), so the tuner can look only at "the bass note
//! 80–400 ms after the kick".
//!
//! Frame lifecycle (one per repaint):
//! 1. The audio callback keeps appending to the capture ring at audio rate,
//!    independent of the UI.
//! 2. The app snapshots the newest audio into a `Shared` (as much as the
//!    visible modules ask for via `Module::lookback`).
//! 3. `Module::update` runs for each visible module in slot order. Modules
//!    compare `frame.end()` (the absolute sample counter) with what they
//!    saw last time and skip work when nothing new arrived.
//! 4. Derived data (spectra, decimated audio) is computed on first request
//!    through `Shared` and memoized, so two modules asking for the same FFT
//!    pay for it once.
//! 5. `Module::graph` draws. Then `Shared::finish` drops every cached
//!    result and hands the FFT plans back for the next frame.

pub mod midi;
pub mod scope;
pub mod spectrogram;
mod spectrum_rows;
pub mod tuner;
pub mod waterfall;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use egui::{Rect, Ui};

use crate::audio::Frame;
use crate::audio::levels::LevelDist;
use crate::audio::spectrum::Fft;
use crate::ui::dynamic_range::DbRange;
use crate::ui::noterange::NoteRange;

/// Which part of the recent audio the analysers should look at.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate {
    /// Absolute sample range `[start, end)`; `None` means free-running:
    /// analyse the latest audio.
    pub window: Option<(u64, u64)>,
}

/// FFT plans (window, twiddles, scratch) by size; they outlive frames.
#[derive(Default)]
pub struct FftPlans(HashMap<usize, Fft>);

/// Results computed at most once per frame.
#[derive(Default)]
struct FrameCache {
    /// (fft size, range start, range end) → power spectrum.
    spectra: HashMap<(usize, u64, u64), Rc<[f32]>>,
    /// Decimation factor → decimated frame.
    #[allow(dead_code)]
    decimated: HashMap<u32, Rc<Frame>>,
}

/// Per-frame state passed through all modules in order.
pub struct Shared {
    pub frame: Frame,
    pub gate: Gate,
    /// True when the stream restarted since the last frame (new device,
    /// rate...): absolute positions from before no longer apply.
    pub reset: bool,
    /// The shared note range every frequency axis shows (the bar above the
    /// status bar). A module may set it (e.g. "fit"); the app keeps it.
    pub range: Cell<NoteRange>,
    /// The dB range every locked [`DynamicRange`](crate::ui::dynamic_range::DynamicRange)
    /// control shares. The app keeps it.
    pub db: Cell<DbRange>,
    /// Recent block-RMS level distribution, for the dB controls' meters.
    pub levels: Rc<LevelDist>,
    plans: RefCell<FftPlans>,
    cache: RefCell<FrameCache>,
}

impl Shared {
    pub fn new(frame: Frame, reset: bool, range: NoteRange, db: DbRange, levels: Rc<LevelDist>, plans: FftPlans) -> Self {
        Self {
            frame,
            gate: Gate::default(),
            reset,
            range: Cell::new(range),
            db: Cell::new(db),
            levels,
            plans: RefCell::new(plans),
            cache: Default::default(),
        }
    }

    /// End of frame: drop every cached result, keep the FFT plans.
    pub fn finish(self) -> FftPlans {
        self.plans.into_inner()
    }

    /// Samples to analyse: the gated window if there is one, otherwise the
    /// latest `free_len` samples.
    pub fn analysis_slice(&self, free_len: usize) -> &[f32] {
        let s = &self.frame.samples;
        match self.gate.window {
            Some((a, b)) => {
                let lo = a.saturating_sub(self.frame.start) as usize;
                let hi = b.saturating_sub(self.frame.start) as usize;
                &s[lo.min(s.len())..hi.min(s.len())]
            }
            None => &s[s.len().saturating_sub(free_len)..],
        }
    }

    /// Hann-windowed power spectrum (`n/2 + 1` bins, full-scale sine ≈ 1.0)
    /// of the audio in the absolute range `[start, end)`. The FFT frame ends
    /// at `end`; anything before `start` or outside the snapshot reads as
    /// silence. `n` must be a power of two. Memoized for this frame.
    pub fn spectrum(&self, n: usize, (start, end): (u64, u64)) -> Rc<[f32]> {
        let start = start.max(end.saturating_sub(n as u64));
        let key = (n, start, end);
        if let Some(s) = self.cache.borrow().spectra.get(&key) {
            return s.clone();
        }
        let f = &self.frame;
        let mut x = vec![0f32; n];
        // x[i] holds absolute sample end - n + i.
        let first = end as i128 - n as i128;
        for (i, v) in x.iter_mut().enumerate() {
            let at = first + i as i128;
            if at >= start as i128 && at >= f.start as i128 && at < f.end() as i128 {
                *v = f.samples[(at - f.start as i128) as usize];
            }
        }
        let power: Rc<[f32]> = {
            let mut plans = self.plans.borrow_mut();
            plans
                .0
                .entry(n)
                .or_insert_with(|| Fft::new(n))
                .power(&x)
                .into()
        };
        self.cache.borrow_mut().spectra.insert(key, power.clone());
        power
    }

    /// The snapshot at `1/factor` of the rate (`factor` a power of two),
    /// for cheaper analysis of low notes. No module uses it yet. `start` counts decimated samples:
    /// decimated sample `j` sits at original sample `j * factor`. Memoized.
    #[allow(dead_code)]
    pub fn decimated(&self, factor: u32) -> Rc<Frame> {
        assert!(factor.is_power_of_two());
        if let Some(d) = self.cache.borrow().decimated.get(&factor) {
            return d.clone();
        }
        let d = if factor == 1 {
            Rc::new(Frame {
                samples: self.frame.samples.clone(),
                rate: self.frame.rate,
                start: self.frame.start,
            })
        } else {
            Rc::new(halve(&self.decimated(factor / 2)))
        };
        self.cache.borrow_mut().decimated.insert(factor, d.clone());
        d
    }
}

/// Halve the rate: a 5-tap binomial low-pass, then every other sample,
/// keeping even absolute indices so repeated halving stays aligned. Gentle
/// (about −6 dB at the new Nyquist), fine for display and pitch of low notes.
#[allow(dead_code)]
fn halve(f: &Frame) -> Frame {
    let x = &f.samples;
    let at = |i: isize| x[i.clamp(0, x.len() as isize - 1) as usize];
    let first = f.start.div_ceil(2);
    let end = f.end() / 2;
    let samples = (first..end)
        .map(|j| {
            let i = (2 * j - f.start) as isize;
            (at(i - 2) + 4.0 * at(i - 1) + 6.0 * at(i) + 4.0 * at(i + 1) + at(i + 2)) / 16.0
        })
        .collect();
    Frame {
        samples,
        rate: f.rate / 2,
        start: first,
    }
}

pub trait Module {
    fn title(&self) -> &'static str;
    /// Write the user-adjustable parameters (not live analysis state).
    fn save_params(&self, storage: &mut dyn eframe::Storage, key: &str);
    /// Restore parameters saved by `save_params`; keeps defaults if absent.
    fn load_params(&mut self, storage: &dyn eframe::Storage, key: &str);
    /// Initial relative display size, before the user adjusts the dividers.
    fn graph_height(&self) -> f32 {
        200.0
    }
    /// Samples of recent audio this module needs in each frame's snapshot
    /// at `rate`. The app copies the largest request among visible modules
    /// (capped by the capture length).
    fn lookback(&self, rate: u32) -> usize {
        rate as usize
    }
    /// Analysis for this frame. Runs whether or not the row is open.
    fn update(&mut self, _shared: &mut Shared) {}
    /// Live pitches (fractional MIDI) to light on the shared range bar.
    fn marks(&self, _out: &mut Vec<f32>) {}
    /// Whether this module has global device controls below the audio toolbar.
    fn has_top_row(&self) -> bool {
        false
    }
    /// Global device controls shown above the workspace layout.
    fn top_row(&mut self, _ui: &mut Ui) {}
    fn params(&mut self, ui: &mut Ui, shared: &Shared);
    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared);
}

/// A module plus its visibility (top bar toggle) and foldout state.
pub struct Slot {
    pub visible: bool,
    pub open: bool,
    pub module: Box<dyn Module>,
}

impl Slot {
    /// Storage key for this module's parameters.
    pub fn params_key(&self) -> String {
        format!("zaudioscope_module.{}", self.module.title())
    }
}

pub fn all() -> Vec<Slot> {
    let slot = |m: Box<dyn Module>| Slot {
        visible: true,
        open: true,
        module: m,
    };
    vec![
        slot(Box::new(scope::Scope::default())),
        slot(Box::new(tuner::Tuner::default())),
        slot(Box::new(spectrogram::Spectrogram::default())),
        slot(Box::new(waterfall::Waterfall::default())),
        slot(Box::new(midi::MidiMonitor::default())),
    ]
}

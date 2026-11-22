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
//! 3. `Module::update` runs for each live module in slot order. A module is
//!    live when it is visible and the page on screen draws it (a solo page
//!    draws only its own analyser), or when it is `always_live`. Modules
//!    compare `frame.end()` (the absolute sample counter) with what they
//!    saw last time and skip work when nothing new arrived; a paused one
//!    catches up when it next runs.
//! 4. Derived data (spectra, decimated audio) is computed on first request
//!    through `Shared` and memoized, so two modules asking for the same FFT
//!    pay for it once.
//! 5. `Module::graph` draws. Then `Shared::finish` drops every cached
//!    result and hands the FFT plans back for the next frame.

pub mod impulse;
pub mod midi;
pub mod note_clock;
mod note_hold;
mod note_input;
pub mod note_spectrum;
mod perf;
mod radial_spectrum;
pub mod scope;
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
use crate::midi::MidiFrame;
use crate::ui::dynamic_range::DbRange;
use crate::ui::noterange::NoteRange;

/// Which part of the recent audio the analysers should look at.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate {
    /// Absolute sample range `[start, end)`; `None` means free-running:
    /// analyse the latest audio.
    pub window: Option<(u64, u64)>,
}

/// FFT plans (window, twiddles, scratch) by size, plus debug counters for
/// the per-frame spectrum cache. Everything here outlives frames.
#[derive(Default)]
pub struct FftPlans {
    plans: HashMap<usize, Fft>,
    /// Work counted so far in the frame being analysed.
    pending: FftCounts,
    /// The last completed frame.
    pub last_frame: FftCounts,
    /// Everything since the app started.
    pub total: FftCounts,
}

/// How FFT work splits between computing spectra and reusing the cache.
#[derive(Default, Clone, Copy, Debug)]
pub struct FftCounts {
    /// Spectra computed (cache miss, or an uncached transform).
    pub computed: u64,
    /// Requests answered from the frame cache without computing.
    pub reused: u64,
    /// Cached spectra thrown away at the end of a frame.
    pub discarded: u64,
}

impl FftCounts {
    fn add(&mut self, other: FftCounts) {
        self.computed += other.computed;
        self.reused += other.reused;
        self.discarded += other.discarded;
    }
}

impl FftPlans {
    /// Two-line debug summary of the FFT cache, for the profiler overlay.
    pub fn report(&self) -> String {
        let (f, t) = (self.last_frame, self.total);
        format!(
            "FFT last frame: {} computed · {} reused · {} discarded\nFFT total: {} computed · {} reused · {} discarded\n",
            f.computed, f.reused, f.discarded, t.computed, t.reused, t.discarded
        )
    }
}

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
    /// MIDI received since the last frame; the app fills it in.
    pub midi: MidiFrame,
    /// Whether the input peak reads above the no-signal floor. The app sets
    /// it from the input meter; while false, displays stop drawing and show
    /// "No signal" instead. Analysis still runs.
    pub signal: bool,
    /// Whether the input is connected and not paused. The app sets it; while
    /// false, displays stop drawing and show "Paused" instead.
    pub connected: bool,
    plans: RefCell<FftPlans>,
    cache: RefCell<FrameCache>,
}

impl Shared {
    pub fn new(
        frame: Frame,
        reset: bool,
        range: NoteRange,
        db: DbRange,
        levels: Rc<LevelDist>,
        plans: FftPlans,
    ) -> Self {
        Self {
            frame,
            gate: Gate::default(),
            reset,
            range: Cell::new(range),
            db: Cell::new(db),
            levels,
            midi: MidiFrame::default(),
            signal: true,
            connected: true,
            plans: RefCell::new(plans),
            cache: Default::default(),
        }
    }

    /// What a display shows in place of its graph, or `None` when it should
    /// draw: "Paused" when no audio can arrive, "No signal" when it is quiet.
    pub fn idle_label(&self) -> Option<&'static str> {
        if !self.connected {
            Some("Paused")
        } else if !self.signal {
            Some("No signal")
        } else {
            None
        }
    }

    /// End of frame: drop every cached result, keep the FFT plans, and roll
    /// this frame's counters into the totals.
    pub fn finish(self) -> FftPlans {
        let discarded = self.cache.into_inner().spectra.len() as u64;
        let mut plans = self.plans.into_inner();
        let mut frame = std::mem::take(&mut plans.pending);
        frame.discarded = discarded;
        plans.total.add(frame);
        plans.last_frame = frame;
        plans
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
            self.plans.borrow_mut().pending.reused += 1;
            return s.clone();
        }
        let power = self.fft_power(&take(&self.frame, n, (start, end)));
        self.cache.borrow_mut().spectra.insert(key, power.clone());
        power
    }

    /// Run the `x.len()`-point plan on `x`.
    fn fft_power(&self, x: &[f32]) -> Rc<[f32]> {
        let mut plans = self.plans.borrow_mut();
        plans.pending.computed += 1;
        plans
            .plans
            .entry(x.len())
            .or_insert_with(|| Fft::new(x.len()))
            .power(x)
            .into()
    }

    /// Power spectrum (`n/2 + 1` bins) of the latest audio at `1/factor` of
    /// the rate, for fine low-frequency resolution: bins are
    /// `rate / factor / n` Hz wide. Only the tail it needs is decimated.
    /// Not memoized (one caller).
    pub fn decimated_spectrum(&self, factor: u32, n: usize) -> Rc<[f32]> {
        assert!(factor.is_power_of_two());
        let f = &self.frame;
        let need = (n + 8) * factor as usize;
        let tail = &f.samples[f.samples.len().saturating_sub(need)..];
        let mut d = Frame {
            samples: tail.to_vec(),
            rate: f.rate,
            start: f.end() - tail.len() as u64,
        };
        let mut k = factor;
        while k > 1 {
            d = halve(&d);
            k /= 2;
        }
        let end = d.end();
        self.fft_power(&take(&d, n, (0, end)))
    }

    /// The largest spectrum some module already computed this frame, as
    /// `(fft size, power, true)`; failing that, a fresh `fallback`-point one
    /// of the latest audio, `(fallback, power, false)`.
    pub fn any_spectrum(&self, fallback: usize) -> (usize, Rc<[f32]>, bool) {
        let cached = self
            .cache
            .borrow()
            .spectra
            .iter()
            .max_by_key(|(k, _)| (k.0, k.2))
            .map(|(k, v)| (k.0, v.clone()));
        match cached {
            Some((n, p)) => {
                self.plans.borrow_mut().pending.reused += 1;
                (n, p, true)
            }
            None => {
                let end = self.frame.end();
                (
                    fallback,
                    self.spectrum(fallback, (end.saturating_sub(fallback as u64), end)),
                    false,
                )
            }
        }
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

/// `n` samples of `f` ending at absolute `end`; anything before `start` or
/// outside the frame reads as silence.
fn take(f: &Frame, n: usize, (start, end): (u64, u64)) -> Vec<f32> {
    // Sample i of the window is absolute position `first + i`. Copy the run
    // that lies inside both the snapshot and `start`; the rest stays silent.
    let first = end as i128 - n as i128;
    let mut out = vec![0.0; n];
    let lo = first.max(start as i128).max(f.start as i128);
    let hi = (end as i128).min(f.end() as i128);
    if lo < hi {
        let src = (lo - f.start as i128) as usize..(hi - f.start as i128) as usize;
        out[(lo - first) as usize..(hi - first) as usize].copy_from_slice(&f.samples[src]);
    }
    out
}

/// Halve the rate: a 5-tap binomial low-pass, then every other sample,
/// keeping even absolute indices so repeated halving stays aligned. Gentle
/// (about −6 dB at the new Nyquist), fine for display and pitch of low notes.
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
    /// Analysis for this frame. Runs while the module is live (see the frame
    /// lifecycle above).
    fn update(&mut self, _shared: &mut Shared) {}
    /// Keeps analysing even when the page on screen does not draw it, because
    /// others read its results through `Shared` (the scope's gate) or it
    /// consumes events that a pause would lose (MIDI note on/off).
    fn always_live(&self) -> bool {
        false
    }
    /// Whether the display shows audio. Those that don't (MIDI) keep drawing
    /// while the input is paused or silent instead of showing the idle label.
    fn uses_audio(&self) -> bool {
        true
    }
    /// Live pitches (fractional MIDI) to light on the shared range bar.
    fn marks(&self, _out: &mut Vec<f32>) {}
    /// Whether this module has controls in the row below the audio toolbar
    /// (beside the MIDI input picker).
    fn has_top_row(&self) -> bool {
        false
    }
    /// Global device controls shown above the workspace layout.
    fn top_row(&mut self, _ui: &mut Ui) {}
    fn params(&mut self, ui: &mut Ui, shared: &Shared);
    /// The full display: background, axes, data and value labels.
    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared);
    /// `Some` for modules whose [`Self::layer`] can be stacked in the
    /// Composite view (mostly drawn across the shared note range; the scope
    /// stretches its time axis over the panel instead).
    /// The value is the default stacking depth (0 = bottom).
    fn layer_depth(&self) -> Option<u8> {
        None
    }
    /// Data only (no background, axes or value labels), for the Composite
    /// view, which draws a shared note grid underneath. Animates like `graph`.
    fn layer(&mut self, _ui: &mut Ui, _rect: Rect, _shared: &Shared) {}
    /// Composite view: `true` draws this analyser in its own pane under the
    /// layers (split off by a divider) with its full [`Self::graph`], instead
    /// of stacking it over them.
    fn composite_pane(&self) -> bool {
        false
    }
    /// Title of a module whose full graph this one draws on top of (with
    /// [`Self::overlay`]) in the stack and in this module's solo page,
    /// while that module is on, instead of in a panel of its own.
    fn stack_over(&self) -> Option<&'static str> {
        None
    }
    /// `true` for analyzers that appear only on their own solo page, never in
    /// the display stack or its inspector.
    fn solo_only(&self) -> bool {
        false
    }
    /// Everything `graph` draws except the background and grid, over the
    /// [`Self::stack_over`] module's full graph in the same rect.
    fn overlay(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared) {
        self.layer(ui, rect, shared)
    }
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
        slot(Box::<scope::Scope>::default()),
        slot(Box::<tuner::Tuner>::default()),
        slot(Box::<note_spectrum::NoteSpectrum>::default()),
        slot(Box::<waterfall::Waterfall>::default()),
        slot(Box::<impulse::Impulse>::default()),
        slot(Box::<midi::MidiMonitor>::default()),
        slot(Box::<note_clock::NoteClock>::default()),
    ]
}

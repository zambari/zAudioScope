//! Analyser modules. Each is a foldout row: parameters on the left, graph
//! on the right. The scope runs first and decides which slice of audio the
//! others analyse (the gate), so the tuner can look only at "the bass note
//! 80–400 ms after the kick".

pub mod scope;
pub mod tuner;
pub mod waterfall;

use egui::{Rect, Ui};

use crate::audio::Frame;

/// Which part of the recent audio the analysers should look at.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate {
    /// Absolute sample range `[start, end)`; `None` means free-running:
    /// analyse the latest audio.
    pub window: Option<(u64, u64)>,
}

/// Per-frame state passed through all modules in order.
pub struct Shared {
    pub frame: Frame,
    pub gate: Gate,
}

impl Shared {
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
}

pub trait Module {
    fn title(&self) -> &'static str;
    fn graph_height(&self) -> f32 {
        200.0
    }
    /// Analysis for this frame. Runs whether or not the row is open.
    fn update(&mut self, _shared: &mut Shared) {}
    fn params(&mut self, ui: &mut Ui);
    fn graph(&mut self, ui: &mut Ui, rect: Rect, shared: &Shared);
}

/// A module plus its visibility (top bar toggle) and foldout state.
pub struct Slot {
    pub visible: bool,
    pub open: bool,
    pub module: Box<dyn Module>,
}

pub fn all() -> Vec<Slot> {
    let slot = |m: Box<dyn Module>| Slot { visible: true, open: true, module: m };
    vec![
        slot(Box::new(scope::Scope::default())),
        slot(Box::new(tuner::Tuner::default())),
        slot(Box::new(waterfall::Waterfall::default())),
    ]
}

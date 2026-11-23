//! A scrollable inspector and a display stack that always fits its viewport.

use std::collections::BTreeMap;

use egui::{Align, Align2, FontId, Id, Layout, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use serde::{Deserialize, Serialize};

use super::outside as noterange_outside;
use super::theme::TabLook;
use super::{icons, noterange, plot, theme, widgets};
use crate::modules::{Module, Shared, Slot, tuner};

const DIVIDER: f32 = 8.0;
const HEADER: f32 = 22.0;
const INSPECTOR_DEFAULT: f32 = 300.0;
const INSPECTOR_MIN: f32 = 270.0;
const DISPLAY_MIN: f32 = 72.0;
/// Height reserved for the tab row above the display column.
const TABS: f32 = 24.0;
/// Share of the Composite page given to its own-pane analyser (the scope)
/// when it is on, until the divider is dragged.
const SCOPE_SHARE: f32 = 0.5;

/// The display column's page, as saved. Solo keeps the title so the choice
/// survives reordering and an analyzer being switched off for a while.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
enum View {
    #[default]
    Stack,
    Composite,
    Solo(String),
}

/// The page a tab stands for this frame (only for analyzers that are on).
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Stack,
    Composite,
    Solo(&'static str),
}

/// One analyzer's place in the Composite view.
#[derive(Clone, Serialize, Deserialize)]
struct MixLayer {
    title: String,
    on: bool,
    opacity: f32,
}

impl MixLayer {
    /// First-time mixer state for an analyzer. Own-pane analysers (the scope)
    /// start off, so the composite stays as it was until asked for.
    fn default_for(title: &str, own_pane: bool) -> Self {
        Self {
            title: title.to_owned(),
            on: !own_pane,
            opacity: 1.0,
        }
    }
}

/// Whether an analyzer takes part in Composite at all: as a stacked layer or
/// in its own pane.
fn in_composite(module: &dyn Module) -> bool {
    module.layer_depth().is_some() || module.composite_pane()
}

/// Whether `title` is an analyzer drawn in its own Composite pane.
fn own_pane(slots: &[Slot], title: &str) -> bool {
    slots
        .iter()
        .any(|s| s.module.title() == title && s.module.composite_pane())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    inspector_width: f32,
    /// Keyed by title so hiding or adding modules preserves their sizes.
    weights: BTreeMap<String, f32>,
    view: View,
    /// Composite layers, bottom first, then own-pane analysers below them.
    mix: Vec<MixLayer>,
    /// Fraction of the Composite page given to the own-pane analyser.
    scope_share: f32,
    /// Stack and inspector order by title, top first; reordered by dragging
    /// a display header. Analysis still runs in slot order (scope first).
    order: Vec<String>,
    /// Interaction feedback is transient; geometry is collected on every repaint.
    #[serde(skip)]
    touched: Option<String>,
    /// Foldout to scroll into view next frame, after a click on a display
    /// that was not already the linked one.
    #[serde(skip)]
    reveal: Option<String>,
    /// Live spectrum outside the note range, drawn into the range bar.
    #[serde(skip)]
    outside: noterange_outside::Outside,
    /// MIDI note glow on the range bar keys.
    #[serde(skip)]
    glow: super::keyglow::KeyGlow,
    /// Live spectrum (and its FFTs) in the note range bar.
    range_fft: bool,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            inspector_width: INSPECTOR_DEFAULT,
            weights: BTreeMap::new(),
            view: View::Stack,
            mix: Vec::new(),
            scope_share: SCOPE_SHARE,
            order: Vec::new(),
            touched: None,
            reveal: None,
            outside: Default::default(),
            glow: Default::default(),
            range_fft: true,
        }
    }
}

impl Workspace {
    pub fn show(
        &mut self,
        ui: &mut Ui,
        slots: &mut [Slot],
        shared: &Shared,
        marks: &[f32],
        profiler: &mut super::profiler::Profiler,
    ) {
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        let area = Rect::from_min_max(
            full.min,
            pos2(
                full.right(),
                (full.bottom() - 8.0 - noterange::BAR_H).max(full.top()),
            ),
        );
        let max_width = (area.width() - 240.0 - DIVIDER).max(0.0);
        let min_width = INSPECTOR_MIN.min(max_width);
        if !self.inspector_width.is_finite() {
            self.inspector_width = INSPECTOR_DEFAULT;
        }
        self.inspector_width = self.inspector_width.clamp(min_width, max_width);
        let border = Rect::from_min_size(
            pos2(area.left() + self.inspector_width, area.top()),
            vec2(DIVIDER, area.height()),
        );
        let response = divider(ui, ui.scope_id().with("inspector_divider"), border, true);
        if response.dragged() {
            self.inspector_width =
                (self.inspector_width + response.drag_delta().x).clamp(min_width, max_width);
        }
        if response.double_clicked() {
            self.inspector_width = INSPECTOR_DEFAULT.clamp(min_width, max_width);
        }
        let inspector = Rect::from_min_max(
            area.min,
            pos2(area.left() + self.inspector_width, area.bottom()),
        );
        let column = Rect::from_min_max(pos2(inspector.right() + DIVIDER, area.top()), area.max);
        let ordered = self.sync_order(slots);
        let visible: Vec<usize> = ordered
            .iter()
            .copied()
            .filter(|&i| slots[i].shown())
            .collect();
        self.sync_mix(slots);
        let layered = visible
            .iter()
            .any(|&i| in_composite(slots[i].module.as_ref()));
        let merged = merges(slots, &visible);
        let overlays: Vec<(usize, &str)> = merged
            .iter()
            .map(|&(h, g)| (h, slots[g].module.title()))
            .collect();
        // A page whose analyzers are all off falls back to the stack.
        let mut current = match &self.view {
            View::Composite if layered => Tab::Composite,
            View::Solo(solo) => visible
                .iter()
                .map(|&i| slots[i].module.title())
                .find(|&t| t == solo)
                .map_or(Tab::Stack, Tab::Solo),
            _ => Tab::Stack,
        };
        let mut tabs = vec![(Tab::Stack, "Stack", TabLook::Normal)];
        // Every analyzer has a solo tab; a switched-off one is dimmed and
        // clicking it switches it back on. One missing its input is inert.
        tabs.extend(ordered.iter().map(|&i| {
            let slot = &slots[i];
            let look = if !slot.available {
                TabLook::Disabled("Connect a MIDI input to show this")
            } else if !slot.visible {
                TabLook::Dim
            } else {
                TabLook::Normal
            };
            (Tab::Solo(slot.module.title()), slot.module.title(), look)
        }));
        if layered {
            tabs.push((Tab::Composite, "Composite", TabLook::Normal));
        }
        let mut tabs_ui = child(ui, "display_tabs", column.with_max_y(column.top() + TABS));
        let before = current;
        theme::tab_strip(&mut tabs_ui, &tabs, &mut current);
        // Only a click rewrites the choice, so re-enabling a hidden solo
        // analyzer brings its page back.
        if current != before {
            self.view = match current {
                Tab::Stack => View::Stack,
                Tab::Composite => View::Composite,
                Tab::Solo(title) => View::Solo(title.to_owned()),
            };
            if let Tab::Solo(title) = current
                && let Some(slot) = slots
                    .iter_mut()
                    .find(|s| s.module.title() == title && !s.visible)
            {
                // Shown from the next frame, once the layout includes it.
                slot.visible = true;
                current = before;
                ui.ctx().request_repaint();
            }
        }
        let displays = column.with_min_y((column.top() + TABS + 4.0).min(column.bottom()));
        let mut left = child(
            ui,
            "inspector",
            Rect::from_min_max(inspector.min + vec2(6.0, 0.0), inspector.max),
        );
        // Reserve a permanent gutter, flush with the inspector's right edge.
        let scroll = &mut left.spacing_mut().scroll;
        scroll.floating = false;
        scroll.bar_width = 8.0;
        scroll.bar_inner_margin = 6.0;
        scroll.bar_outer_margin = 0.0;
        scroll.foreground_color = false;
        let mut anchors = BTreeMap::new();
        let mut viewport = Rect::NOTHING;
        egui::ScrollArea::vertical()
            .id_salt("parameters")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .auto_shrink([false, false])
            .show(&mut left, |ui| {
                // Keep labels readable while letting tracks grow with the inspector.
                ui.spacing_mut().slider_width = (ui.available_width() - 150.0).clamp(90.0, 260.0);
                viewport = ui.clip_rect();
                for &i in &ordered {
                    let slot = &mut slots[i];
                    let title = slot.module.title();
                    // Solo and Composite focus the inspector on what they show.
                    let shown = match current {
                        Tab::Stack => !slot.module.solo_only(),
                        Tab::Composite => in_composite(slot.module.as_ref()),
                        // A solo overlay also shows the graph it draws over.
                        Tab::Solo(solo) => solo == title || overlays.contains(&(i, solo)),
                    };
                    if !shown || !slot.available {
                        continue;
                    }
                    let start = profiler.start();
                    let section = ui.push_id(title, |ui| {
                        let anchor = widgets::module_header(
                            ui,
                            slot.module.title(),
                            &mut slot.open,
                            &mut slot.visible,
                        );
                        anchors.insert(title.to_owned(), anchor);
                        if slot.open {
                            // Reserve the background before painting controls so its
                            // padding does not move or resize the parameter layout.
                            let background = ui.painter().add(egui::Shape::Noop);
                            let content = ui.scope(|ui| {
                                ui.set_min_width(ui.available_width());
                                ui.add_enabled_ui(slot.visible, |ui| {
                                    if !slot.visible {
                                        ui.set_opacity(0.4);
                                    }
                                    // Dim fills; the dB range opts back into ACCENT.
                                    theme::with_fill(ui, theme::ACCENT_DIM, |ui| {
                                        ui.push_id("params", |ui| slot.module.params(ui, shared))
                                    });
                                });
                            });
                            if !slot.visible {
                                ui.painter().set(
                                    background,
                                    egui::Shape::rect_filled(
                                        content.response.rect.expand(3.0),
                                        0.0,
                                        egui::Color32::from_black_alpha(77),
                                    ),
                                );
                            }
                            ui.add_space(6.0);
                        }
                        ui.separator();
                    });
                    profiler.record("Foldout", title, start);
                    if self.reveal.as_deref() == Some(title) {
                        self.reveal = None;
                        ui.scroll_to_rect(section.response.rect, Some(Align::TOP));
                    }
                    if pressed_in(ui, section.response.rect) {
                        self.touched = Some(title.to_owned());
                    }
                    // Sits in the gutter left of the foldout, inside the
                    // scroll area's clip margin.
                    if current == Tab::Stack && self.touched.as_deref() == Some(title) {
                        let rect = section.response.rect;
                        theme::selected_edge(ui.painter(), rect.left() - 2.0, rect.y_range());
                    }
                }
            });
        let panels = match current {
            // The stack keeps switched-off analyzers as folded header strips.
            // Overlays ride in their host's panel instead of a row of their own.
            Tab::Stack => {
                let rows: Vec<usize> = ordered
                    .iter()
                    .copied()
                    .filter(|&i| {
                        slots[i].available
                            && !merged.iter().any(|m| m.1 == i)
                            && !slots[i].module.solo_only()
                    })
                    .collect();
                self.displays(ui, displays, slots, &rows, &merged, true, shared, profiler)
            }
            Tab::Solo(title) => {
                let solo: Vec<usize> = visible
                    .into_iter()
                    .filter(|&i| slots[i].module.title() == title)
                    .collect();
                // An overlay's solo page draws it over its host; a host's is plain.
                match merged.iter().find(|m| solo.contains(&m.1)) {
                    Some(&m) => {
                        self.displays(ui, displays, slots, &[m.0], &[m], false, shared, profiler)
                    }
                    None => self.displays(ui, displays, slots, &solo, &[], false, shared, profiler),
                }
            }
            Tab::Composite => self.composite(ui, displays, slots, shared, profiler),
        };
        if let Some(title) = &self.touched
            && let Some(&(start, turn_x)) = anchors.get(title)
            && let Some(panel) = panels.get(title)
        {
            let end = panel.left_center();
            let y = start.y.clamp(viewport.top(), viewport.bottom());
            // A header scrolled out of view keeps only the corner, pointing at it.
            let points = if y == start.y {
                vec![
                    pos2(start.x.min(turn_x), y),
                    pos2(turn_x, y),
                    pos2(turn_x, end.y),
                    end,
                ]
            } else {
                vec![pos2(turn_x, y), pos2(turn_x, end.y), end]
            };
            let painter = ui.painter().with_clip_rect(area.intersect(ui.clip_rect()));
            let stroke = Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.35));
            // A parameter guide shares this route; pause ours so they don't overlap.
            // Only the stack needs the leader: solo and composite show one display.
            if current == Tab::Stack && !plot::links_pending(ui) {
                painter.extend(egui::Shape::dashed_line(&points, stroke, 5.0, 4.0));
            }
            theme::selected_edge(&painter, panel.left(), panel.y_range());
        }
        // Draw after both columns so graphs cannot cover a cross-panel guide.
        plot::paint_links(ui, area);
        let range_rect = Rect::from_min_max(pos2(full.left(), area.bottom() + 8.0), full.max);
        let mut range_ui = child(ui, "note_range", range_rect);
        let mut range = shared.range.get();
        let readout_width = self.inspector_width + DIVIDER - range_ui.spacing().item_spacing.x;
        if self.range_fft {
            // Out-of-range spectrum: a fine decimated FFT for the low end, plus
            // the largest FFT an analyser already ran (else our own) above it.
            let rate = shared.frame.rate;
            let start = profiler.start();
            let (n, coarse, reused) = shared.any_spectrum(noterange_outside::COARSE_FFT);
            profiler.record(
                "Range",
                if reused {
                    "coarse FFT shared"
                } else {
                    "coarse FFT own"
                },
                start,
            );
            let start = profiler.start();
            let (d, fn_) = (
                noterange_outside::FINE_DECIMATE,
                noterange_outside::FINE_FFT,
            );
            let fine = shared.decimated_spectrum(d, fn_);
            profiler.record("Range", "fine FFT /16", start);
            let start = profiler.start();
            let fine_rate = rate as f32 / d as f32;
            self.outside.update(
                &noterange_outside::Source::new(
                    &fine,
                    fine_rate,
                    fn_,
                    fine_rate * noterange_outside::FINE_TOP,
                ),
                &noterange_outside::Source::new(&coarse, rate as f32, n, rate as f32 / 2.0),
                rate,
                range,
                n,
                reused,
            );
            profiler.record("Range", "side stats", start);
        }
        let start = profiler.start();
        // Live pitch markers only where a pitch source is the point of the page.
        let marks = if matches!(current, Tab::Stack | Tab::Solo(tuner::TITLE)) {
            marks
        } else {
            &[]
        };
        let outside = self.range_fft.then_some(&self.outside);
        if self.glow.update(&shared.midi, None) {
            ui.ctx().request_repaint();
        }
        if noterange::bar(
            &mut range_ui,
            &mut range,
            &mut self.range_fft,
            marks,
            readout_width,
            outside,
            &self.glow,
        ) {
            shared.range.set(range);
        }
        profiler.record("Range", "bar draw", start);
    }

    /// Which slots the page drawn this frame asks for. A solo page asks for
    /// its analyzer and the host it draws over; Stack and Composite ask for
    /// everything (the caller still limits this to visible slots).
    pub fn requested(&self, slots: &[Slot]) -> Vec<bool> {
        let all = vec![true; slots.len()];
        let View::Solo(title) = &self.view else {
            return all;
        };
        let visible: Vec<usize> = (0..slots.len()).filter(|&i| slots[i].shown()).collect();
        let Some(solo) = visible
            .iter()
            .copied()
            .find(|&i| slots[i].module.title() == title.as_str())
        else {
            return all;
        };
        let mut wanted = vec![false; slots.len()];
        wanted[solo] = true;
        if let Some(&(host, _)) = merges(slots, &visible).iter().find(|m| m.1 == solo) {
            wanted[host] = true;
        }
        wanted
    }

    /// Keep one order entry per analyzer (new ones go last) and return slot
    /// indices in that order.
    fn sync_order(&mut self, slots: &[Slot]) -> Vec<usize> {
        let index = |title: &str| slots.iter().position(|s| s.module.title() == title);
        let mut seen = std::collections::BTreeSet::new();
        self.order
            .retain(|t| index(t).is_some() && seen.insert(t.clone()));
        for slot in slots {
            if !self.order.iter().any(|t| t == slot.module.title()) {
                self.order.push(slot.module.title().to_owned());
            }
        }
        self.order.iter().filter_map(|t| index(t)).collect()
    }

    /// Keep one mix entry per Composite analyzer, stacked by depth: the
    /// opaque waterfall at the bottom, then spectrum, tuner, and MIDI notes on
    /// top. Own-pane analysers (the scope) sort last, below a separator.
    fn sync_mix(&mut self, slots: &[Slot]) {
        let info = |title: &str| {
            slots.iter().find(|s| s.module.title() == title).map(|s| {
                (
                    s.module.layer_depth().unwrap_or(0),
                    s.module.composite_pane(),
                )
            })
        };
        self.mix.retain(|l| info(&l.title).is_some());
        for slot in slots {
            let title = slot.module.title();
            if in_composite(slot.module.as_ref()) && !self.mix.iter().any(|l| l.title == title) {
                self.mix
                    .push(MixLayer::default_for(title, slot.module.composite_pane()));
            }
        }
        // Own-pane entries first in ascending order, so they come last in the
        // mixer (which lists top first) and at the bottom of the page.
        self.mix.sort_by_key(|l| {
            let (depth, pane) = info(&l.title).unwrap_or((0, false));
            (!pane, depth)
        });
        for l in &mut self.mix {
            l.opacity = if l.opacity.is_finite() {
                l.opacity.clamp(0.0, 1.0)
            } else {
                1.0
            };
        }
    }

    /// Every layerable analyzer that is on, drawn bottom to top over a shared
    /// note grid in one panel. An own-pane analyser (the scope) gets its own
    /// pane under the layers, split off by a draggable divider. The Layers
    /// button opens the mixer.
    fn composite(
        &mut self,
        ui: &mut Ui,
        area: Rect,
        slots: &mut [Slot],
        shared: &Shared,
        profiler: &mut super::profiler::Profiler,
    ) -> BTreeMap<String, Rect> {
        let mut panels = BTreeMap::new();
        // The own-pane analyser shown this frame, if any.
        let pane = slots.iter().position(|s| {
            s.shown()
                && s.module.composite_pane()
                && self.mix.iter().any(|l| l.on && l.title == s.module.title())
        });
        let Some(pane) = pane else {
            self.layers(ui, area, slots, shared, profiler, &mut panels);
            return panels;
        };
        if !self.scope_share.is_finite() {
            self.scope_share = SCOPE_SHARE;
        }
        // Same split as the stack: a divider between the layers and the pane.
        let avail = (area.height() - DIVIDER).max(0.0);
        let minimum = DISPLAY_MIN.min(avail * 0.5);
        let limit = |h: f32| h.clamp(minimum, (avail - minimum).max(minimum));
        let lower = limit(avail * self.scope_share.clamp(0.0, 1.0));
        let split = area.bottom() - lower - DIVIDER;
        let divider_rect =
            Rect::from_min_size(pos2(area.left(), split), vec2(area.width(), DIVIDER));
        let response = divider(
            ui,
            ui.scope_id().with("composite_divider"),
            divider_rect,
            false,
        );
        // A drag takes effect from the next frame, when the panes are laid out again.
        if response.dragged() || response.double_clicked() {
            let next = if response.double_clicked() {
                avail * 0.5
            } else {
                lower - response.drag_delta().y
            };
            self.scope_share = if avail > 0.0 {
                limit(next) / avail
            } else {
                SCOPE_SHARE
            };
        }
        let top = Rect::from_min_max(area.min, pos2(area.right(), split));
        let below = Rect::from_min_max(pos2(area.left(), split + DIVIDER), area.max);
        self.layers(ui, top, slots, shared, profiler, &mut panels);
        scope_pane(ui, below, &mut slots[pane], shared, profiler, &mut panels);
        panels
    }

    /// The stacked layers, drawn over a shared note grid in `area`.
    fn layers(
        &mut self,
        ui: &mut Ui,
        area: Rect,
        slots: &mut [Slot],
        shared: &Shared,
        profiler: &mut super::profiler::Profiler,
        panels: &mut BTreeMap<String, Rect>,
    ) {
        ui.painter()
            .rect_filled(area, 2.0, ui.visuals().extreme_bg_color);
        let graph = Rect::from_min_max(
            pos2(area.left() + 1.0, area.top() + HEADER),
            area.max - vec2(1.0, 1.0),
        );
        let sized = graph.height() >= 44.0 && graph.width() >= 24.0;
        // The stack waits for its slowest layer (the waterfall's history).
        let wait = self
            .mix
            .iter()
            .filter(|l| l.on && l.opacity > 0.0 && !own_pane(slots, &l.title))
            .filter_map(|l| {
                slots
                    .iter()
                    .find(|s| s.shown() && s.module.title() == l.title)
            })
            .map(|s| s.module.idle_wait_s())
            .fold(crate::ui::meter::NO_SIGNAL_HOLD_S, f32::max);
        if sized && let Some(label) = shared.idle_label(wait) {
            widgets::placeholder(ui, graph, label);
        } else if sized {
            plot::note_grid(
                &ui.painter().with_clip_rect(graph),
                graph,
                shared.range.get(),
            );
            let stacked: Vec<(String, f32)> = self
                .mix
                .iter()
                .filter(|l| l.on && l.opacity > 0.0 && !own_pane(slots, &l.title))
                .map(|l| (l.title.clone(), l.opacity))
                .collect();
            for (title, opacity) in stacked {
                let Some(slot) = slots
                    .iter_mut()
                    .find(|s| s.shown() && s.module.title() == title)
                else {
                    continue;
                };
                let start = profiler.start();
                let mut pane = child(ui, ("composite", slot.module.title()), graph);
                pane.set_opacity(opacity);
                slot.module.layer(&mut pane, graph, shared);
                panels.insert(slot.module.title().to_owned(), area);
                profiler.record("Display", slot.module.title(), start);
            }
        }
        ui.painter().text(
            area.left_top() + vec2(8.0, HEADER * 0.5),
            Align2::LEFT_CENTER,
            "Composite",
            FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
        let mut header = ui.new_child(
            UiBuilder::new()
                .id_salt("composite_header")
                .max_rect(area.with_max_y(area.top() + HEADER).shrink2(vec2(4.0, 1.0)))
                .layout(Layout::right_to_left(Align::Center)),
        );
        widgets::popup_button(
            &mut header,
            "Layers",
            "Layer visibility and opacity",
            |ui| self.mixer(ui, slots),
        );
    }

    /// One row per layer, topmost first: on/off and opacity. Own-pane
    /// analysers follow a separator, since they are not stacked over the layers.
    fn mixer(&mut self, ui: &mut Ui, slots: &[Slot]) {
        let mut separated = false;
        for layer in self.mix.iter_mut().rev() {
            if !slots
                .iter()
                .any(|s| s.available && s.module.title() == layer.title)
            {
                continue;
            }
            let pane = own_pane(slots, &layer.title);
            if pane && !separated {
                ui.separator();
                separated = true;
            }
            let on = slots
                .iter()
                .any(|s| s.shown() && s.module.title() == layer.title);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(on, |ui| {
                    ui.add(widgets::flicktoggle(&mut layer.on));
                    ui.add_sized(vec2(84.0, 18.0), egui::Label::new(&layer.title).truncate());
                    // A pane has its own space, so only stacked layers fade.
                    if !pane {
                        ui.add_enabled(
                            layer.on,
                            egui::Slider::new(&mut layer.opacity, 0.0..=1.0)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                        );
                    }
                })
                .response
                .on_disabled_hover_text("Turn this analyzer on to layer it");
            });
        }
    }

    /// Lay out `rows` (slot indices) as a resizable vertical stack. In the
    /// `stack` page, analyzers that are off stay as folded header strips; a
    /// header's arrow folds or unfolds it and dragging a header reorders.
    #[allow(clippy::too_many_arguments)]
    /// `merged` (host, overlay) pairs: the overlay draws over its host's row.
    fn displays(
        &mut self,
        ui: &mut Ui,
        area: Rect,
        slots: &mut [Slot],
        rows: &[usize],
        merged: &[(usize, usize)],
        stack: bool,
        shared: &Shared,
        profiler: &mut super::profiler::Profiler,
    ) -> BTreeMap<String, Rect> {
        let mut panels = BTreeMap::new();
        if rows.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                "Turn on an analyzer to show its display",
                FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return panels;
        }
        // Row positions (into `rows`) of the panels that get a graph.
        let expanded: Vec<usize> = (0..rows.len())
            .filter(|&r| slots[rows[r]].visible)
            .collect();
        let weights: Vec<f32> = expanded
            .iter()
            .map(|&r| {
                let module = &slots[rows[r]].module;
                let weight = self
                    .weights
                    .entry(module.title().to_owned())
                    .or_insert(module.graph_height());
                if !weight.is_finite() || *weight <= 0.0 {
                    *weight = module.graph_height();
                }
                *weight
            })
            .collect();
        let folded = (rows.len() - expanded.len()) as f32;
        let available =
            (area.height() - DIVIDER * (rows.len() - 1) as f32 - HEADER * folded).max(0.0);
        let mut heights = vec![HEADER; rows.len()];
        for (&r, size) in expanded
            .iter()
            .zip(proportional_sizes(&weights, available, DISPLAY_MIN))
        {
            heights[r] = size;
        }
        let minimum = DISPLAY_MIN.min(available / expanded.len().max(1) as f32);
        let mut y = area.top();
        let mut resized = false;
        for gap in 0..rows.len() - 1 {
            y += heights[gap];
            // A divider resizes the nearest open panels on either side, so
            // folded strips in between just ride along.
            let above = expanded.iter().rev().copied().find(|&r| r <= gap);
            let below = expanded.iter().copied().find(|&r| r > gap);
            if let (Some(a), Some(b)) = (above, below) {
                let rect = Rect::from_min_size(pos2(area.left(), y), vec2(area.width(), DIVIDER));
                let id = ui.scope_id().with((
                    "display_divider",
                    slots[rows[gap]].module.title(),
                    slots[rows[gap + 1]].module.title(),
                ));
                let response = divider(ui, id, rect, false);
                if response.dragged() || response.double_clicked() {
                    let delta = if response.double_clicked() {
                        (heights[b] - heights[a]) * 0.5
                    } else {
                        response.drag_delta().y
                    };
                    let delta = delta.clamp(
                        (minimum - heights[a]).min(0.0),
                        (heights[b] - minimum).max(0.0),
                    );
                    heights[a] += delta;
                    heights[b] -= delta;
                    y += delta;
                    resized = true;
                }
            }
            y += DIVIDER;
        }
        if resized && available > 0.0 {
            let total: f32 = weights.iter().sum();
            for &r in &expanded {
                self.weights.insert(
                    slots[rows[r]].module.title().to_owned(),
                    heights[r] / available * total,
                );
            }
        }
        let mut rects = Vec::with_capacity(rows.len());
        // (row being dragged, pointer y, released this frame)
        let mut drag = None;
        y = area.top();
        for (r, (&i, &height)) in rows.iter().zip(&heights).enumerate() {
            let start = profiler.start();
            let guest = merged.iter().find(|m| m.0 == i).map(|m| m.1);
            let guest_title = guest.map(|g| slots[g].module.title());
            let slot = &mut slots[i];
            let title = slot.module.title();
            let rect = Rect::from_min_size(pos2(area.left(), y), vec2(area.width(), height));
            rects.push(rect);
            panels.insert(title.to_owned(), rect);
            if let Some(g) = guest_title {
                panels.insert(g.to_owned(), rect);
            }
            let mut pane = child(ui, title, rect);
            let fill = ui.visuals().extreme_bg_color;
            pane.painter().rect_filled(
                rect,
                2.0,
                if slot.visible {
                    fill
                } else {
                    fill.gamma_multiply(0.5)
                },
            );
            // Open stack panels float their header as a corner badge over the
            // graph instead of spending a whole row on it.
            let badge = stack && slot.visible;
            // At the minimum window height, use an overlaid title so the
            // axes still have room even with every analyzer visible.
            let header = if badge {
                0.0
            } else if !slot.visible || height >= DISPLAY_MIN {
                HEADER
            } else {
                0.0
            };
            let graph = Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.top() + header),
                rect.max - vec2(1.0, 1.0),
            );
            // Tiny windows still fit; don't pass inverted plot bounds to modules.
            if slot.visible && graph.height() >= 44.0 && graph.width() >= 24.0 {
                if let Some(label) = shared
                    .idle_label(slot.module.idle_wait_s())
                    .filter(|_| slot.module.uses_audio())
                {
                    widgets::placeholder(&pane, graph, label);
                } else {
                    slot.module.graph(&mut pane, graph, shared);
                    if let Some(g) = guest {
                        slots[g].module.overlay(&mut pane, graph, shared);
                    }
                }
            }
            let slot = &mut slots[i];
            let name = match guest_title {
                Some(g) => format!("{title} + {g}"),
                None => title.to_owned(),
            };
            let color = ui.visuals().weak_text_color();
            let strip = if header > 0.0 || badge {
                let font = FontId::proportional(11.0);
                let strip = if badge {
                    // Arrow + name, sized to the text, on a translucent backdrop.
                    let width = pane
                        .painter()
                        .layout_no_wrap(name.clone(), font.clone(), color)
                        .size()
                        .x;
                    let strip = Rect::from_min_size(rect.min, vec2(22.0 + width + 8.0, HEADER));
                    pane.painter()
                        .rect_filled(strip, 2.0, fill.gamma_multiply(0.75));
                    strip
                } else {
                    rect.with_max_y(rect.top() + HEADER)
                };
                let mut text_x = 8.0;
                if stack {
                    icons::disclosure(
                        pane.painter(),
                        strip.left_center() + vec2(12.0, 0.0),
                        slot.visible,
                        color,
                    );
                    text_x = 22.0;
                }
                let name = pane.painter().text(
                    strip.left_center() + vec2(text_x, 0.0),
                    Align2::LEFT_CENTER,
                    name,
                    font,
                    color,
                );
                (strip, name.expand2(vec2(6.0, 4.0)))
            } else {
                let tag = plot::tag(
                    pane.painter(),
                    rect,
                    rect.right_top() + vec2(-6.0, 2.0),
                    Align2::RIGHT_TOP,
                    name,
                    color,
                );
                (tag, tag)
            };
            if stack {
                let (strip, name) = strip;
                let response = pane
                    .interact(
                        strip,
                        pane.scope_id().with("panel_header"),
                        Sense::click_and_drag(),
                    )
                    .on_hover_text(
                        "Click the arrow to fold (switches the analyzer off) · drag to reorder",
                    );
                let on_name = |pos: Option<egui::Pos2>| pos.is_some_and(|p| name.contains(p));
                if response.hovered() && on_name(response.hover_pos()) {
                    pane.painter().rect_filled(
                        name,
                        2.0,
                        ui.visuals()
                            .widgets
                            .hovered
                            .weak_bg_fill
                            .gamma_multiply(0.6),
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                } else if response.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
                if response.clicked() && on_name(response.interact_pointer_pos()) {
                    slot.visible = !slot.visible;
                    slot.open = slot.visible;
                }
                if response.dragged() || response.drag_stopped() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    if let Some(pos) = response.interact_pointer_pos() {
                        drag = Some((r, pos.y, response.drag_stopped()));
                    }
                }
            }
            if pressed_in(ui, rect) {
                self.touched = Some(title.to_owned());
            }
            profiler.record("Display", title, start);
            y += height + DIVIDER;
        }
        if let Some((from, pointer, released)) = drag {
            // Insert before the first other row whose middle is below the pointer.
            let to = (0..rows.len())
                .filter(|&r| r != from)
                .position(|r| rects[r].center().y > pointer)
                .unwrap_or(rows.len() - 1);
            let others: Vec<usize> = (0..rows.len()).filter(|&r| r != from).collect();
            let line_y = match others.get(to) {
                Some(&r) => rects[r].top() - DIVIDER * 0.5,
                None => {
                    others
                        .last()
                        .map_or(rects[from].bottom(), |&r| rects[r].bottom())
                        + DIVIDER * 0.5
                }
            };
            let painter = ui.painter();
            painter.rect_stroke(
                rects[from],
                2.0,
                Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.6)),
                egui::StrokeKind::Inside,
            );
            painter.hline(
                area.x_range(),
                line_y,
                Stroke::new(2.0, theme::ACCENT_BRIGHT),
            );
            if released {
                let moved = slots[rows[from]].module.title().to_owned();
                // Positions in `self.order` of the rows shown, so a solo page
                // or a missing slot never scrambles the rest.
                let mut titles: Vec<String> = rows
                    .iter()
                    .map(|&i| slots[i].module.title().to_owned())
                    .collect();
                titles.remove(from);
                titles.insert(to, moved);
                let mut shown = titles.into_iter();
                for t in self.order.iter_mut() {
                    if rows.iter().any(|&i| slots[i].module.title() == t) {
                        *t = shown.next().unwrap_or_default();
                    }
                }
            }
        }
        panels
    }
}

/// (host, overlay) slot pairs among `visible`: each overlay whose
/// [`stack_over`](crate::modules::Module::stack_over) target is also on.
fn merges(slots: &[Slot], visible: &[usize]) -> Vec<(usize, usize)> {
    visible
        .iter()
        .filter_map(|&g| {
            let host = slots[g].module.stack_over()?;
            let h = visible
                .iter()
                .copied()
                .find(|&h| h != g && slots[h].module.title() == host)?;
            Some((h, g))
        })
        .collect()
}

/// An own-pane analyser's full graph in its own panel under the Composite
/// layers, with its name in the header strip like a stack display.
fn scope_pane(
    ui: &mut Ui,
    rect: Rect,
    slot: &mut Slot,
    shared: &Shared,
    profiler: &mut super::profiler::Profiler,
    panels: &mut BTreeMap<String, Rect>,
) {
    let title = slot.module.title();
    panels.insert(title.to_owned(), rect);
    ui.painter()
        .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let graph = Rect::from_min_max(
        pos2(rect.left() + 1.0, rect.top() + HEADER),
        rect.max - vec2(1.0, 1.0),
    );
    let mut pane = child(ui, ("composite_pane", title), rect);
    if graph.height() >= 44.0 && graph.width() >= 24.0 {
        if let Some(label) = shared.idle_label(slot.module.idle_wait_s()) {
            widgets::placeholder(&pane, graph, label);
        } else {
            let start = profiler.start();
            slot.module.graph(&mut pane, graph, shared);
            profiler.record("Display", title, start);
        }
    }
    pane.painter().text(
        rect.left_top() + vec2(8.0, HEADER * 0.5),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(11.0),
        ui.visuals().weak_text_color(),
    );
}

fn child(ui: &mut Ui, salt: impl egui::AsIdSalt, rect: Rect) -> Ui {
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(salt)
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child
}

/// Observe a primary press inside `rect` without taking interaction from the
/// controls drawn there; links an inspector foldout with its display.
fn pressed_in(ui: &Ui, rect: Rect) -> bool {
    ui.input(|i| i.pointer.primary_pressed())
        && ui
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|pos| rect.intersect(ui.clip_rect()).contains(pos))
        && ui.rect_contains_pointer(rect)
}

/// Shared visual and hit target for both axes. Double-click resets the split.
fn divider(ui: &Ui, id: Id, rect: Rect, vertical: bool) -> egui::Response {
    let cursor = if vertical {
        egui::CursorIcon::ResizeHorizontal
    } else {
        egui::CursorIcon::ResizeVertical
    };
    let response = ui
        .interact(rect, id, Sense::click_and_drag())
        .on_hover_cursor(cursor);
    let active = response.hovered() || response.dragged();
    let color = if active {
        theme::ACCENT_BRIGHT
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    let painter = ui.painter();
    if vertical {
        painter.vline(rect.center().x, rect.y_range(), Stroke::new(1.0, color));
    } else {
        painter.hline(rect.x_range(), rect.center().y, Stroke::new(1.0, color));
    }
    for offset in [-5.0, 0.0, 5.0] {
        let offset = if vertical {
            vec2(0.0, offset)
        } else {
            vec2(offset, 0.0)
        };
        painter.circle_filled(rect.center() + offset, 1.2, color);
    }
    response.on_hover_text(if vertical {
        "Drag to resize inspector · double-click to reset"
    } else {
        "Drag to resize displays · double-click for equal sizes"
    })
}

/// Proportional allocation with a floor. If space is tight, every pane fits.
fn proportional_sizes(weights: &[f32], available: f32, minimum: f32) -> Vec<f32> {
    if weights.is_empty() {
        return Vec::new();
    }
    let minimum = minimum.min(available / weights.len() as f32);
    let mut sizes = vec![0.0; weights.len()];
    let mut remaining = available;
    let mut pending: Vec<usize> = (0..weights.len()).collect();
    while !pending.is_empty() {
        let total: f32 = pending.iter().map(|&i| weights[i]).sum();
        let small: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|&i| remaining * weights[i] / total < minimum)
            .collect();
        if small.is_empty() {
            for i in pending {
                sizes[i] = remaining * weights[i] / total;
            }
            break;
        }
        for &i in &small {
            sizes[i] = minimum;
            remaining -= minimum;
        }
        pending.retain(|i| !small.contains(i));
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        audio::Frame,
        modules::{FftPlans, Module},
        ui::noterange::NoteRange,
    };
    use std::{cell::RefCell, rc::Rc};

    struct Probe {
        title: &'static str,
        graphs: Rc<RefCell<Vec<Rect>>>,
    }

    impl Module for Probe {
        fn title(&self) -> &'static str {
            self.title
        }
        fn save_params(&self, _: &mut dyn eframe::Storage, _: &str) {}
        fn load_params(&mut self, _: &dyn eframe::Storage, _: &str) {}
        fn params(&mut self, ui: &mut Ui, _: &Shared) {
            for _ in 0..30 {
                ui.label("Inspector control");
            }
        }
        fn graph(&mut self, _: &mut Ui, rect: Rect, _: &Shared) {
            self.graphs.borrow_mut().push(rect);
        }
    }

    #[test]
    fn workspace_dragging_and_foldouts_keep_displays_in_view() {
        let ctx = egui::Context::default();
        let graphs = Rc::new(RefCell::new(Vec::new()));
        let mut slots: Vec<Slot> = ["One", "Two", "Three"]
            .into_iter()
            .map(|title| Slot {
                visible: true,
                available: true,
                open: true,
                module: Box::new(Probe {
                    title,
                    graphs: graphs.clone(),
                }),
            })
            .collect();
        let shared = Shared::new(
            Frame {
                samples: Vec::new(),
                rate: 48000,
                start: 0,
            },
            false,
            NoteRange::default(),
            crate::ui::dynamic_range::DbRange::default(),
            Default::default(),
            FftPlans::default(),
        );
        let mut workspace = Workspace::default();
        let frame = |workspace: &mut Workspace, slots: &mut [Slot], events: Vec<egui::Event>| {
            graphs.borrow_mut().clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 700.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        workspace.show(ui, slots, &shared, &[], &mut Default::default())
                    });
                },
            );
            output.textures_delta.clear();
            graphs.borrow().clone()
        };
        let initial = frame(&mut workspace, &mut slots, vec![]);
        assert_eq!(initial.len(), 3);
        assert!(initial.last().unwrap().bottom() < 700.0);
        slots[0].open = false;
        assert_eq!(frame(&mut workspace, &mut slots, vec![]), initial);

        let border = pos2(initial[0].center().x, initial[0].bottom() + 5.0);
        let pointer = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(border), pointer(border, true)],
        );
        let moved = border + vec2(0.0, 45.0);
        let resized = frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(moved)],
        );
        frame(&mut workspace, &mut slots, vec![pointer(moved, false)]);
        assert!(resized[0].height() > initial[0].height() + 30.0);
        assert!(resized[1].height() < initial[1].height() - 30.0);
        assert!((resized[2].height() - initial[2].height()).abs() < 0.01);

        let divider = pos2(resized[0].left() - 5.0, 100.0);
        frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(divider), pointer(divider, true)],
        );
        let moved = divider + vec2(60.0, 0.0);
        let wider = frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(moved)],
        );
        frame(&mut workspace, &mut slots, vec![pointer(moved, false)]);
        assert!(wider[0].left() > resized[0].left() + 50.0);
        slots[1].visible = false;
        assert_eq!(frame(&mut workspace, &mut slots, vec![]).len(), 2);
        slots[1].visible = true;
        let restored = frame(&mut workspace, &mut slots, vec![]);
        assert!((restored[0].height() - wider[0].height()).abs() < 0.01);

        // Drag the first header (away from its name) below the last panel.
        let grab = pos2(restored[0].right() - 40.0, restored[0].top() - HEADER * 0.5);
        let drop = pos2(grab.x, restored[2].bottom() - 5.0);
        frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(grab), pointer(grab, true)],
        );
        for step in 1..=4 {
            let at = grab + (drop - grab) * (step as f32 / 4.0);
            frame(
                &mut workspace,
                &mut slots,
                vec![egui::Event::PointerMoved(at)],
            );
        }
        frame(&mut workspace, &mut slots, vec![pointer(drop, false)]);
        assert_eq!(workspace.order, ["Two", "Three", "One"]);

        // Clicking the top header's name folds it: off, inspector closed.
        let top = frame(&mut workspace, &mut slots, vec![]);
        let name = pos2(top[0].left() + 24.0, top[0].top() - HEADER * 0.5);
        frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(name), pointer(name, true)],
        );
        frame(&mut workspace, &mut slots, vec![pointer(name, false)]);
        assert!(!slots[1].visible && !slots[1].open);
        let folded = frame(&mut workspace, &mut slots, vec![]);
        assert_eq!(folded.len(), 2);
        // The folded strip stays on top, so the remaining panels start below it.
        assert!(folded[0].top() > top[0].top() + DIVIDER);
        frame(
            &mut workspace,
            &mut slots,
            vec![egui::Event::PointerMoved(name), pointer(name, true)],
        );
        frame(&mut workspace, &mut slots, vec![pointer(name, false)]);
        assert!(slots[1].visible && slots[1].open);

        for slot in &mut slots {
            slot.visible = false;
        }
        assert!(frame(&mut workspace, &mut slots, vec![]).is_empty());
    }

    struct Layered {
        title: &'static str,
        depth: Option<u8>,
        drawn: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Module for Layered {
        fn title(&self) -> &'static str {
            self.title
        }
        fn save_params(&self, _: &mut dyn eframe::Storage, _: &str) {}
        fn load_params(&mut self, _: &dyn eframe::Storage, _: &str) {}
        fn params(&mut self, _: &mut Ui, _: &Shared) {}
        fn graph(&mut self, _: &mut Ui, _: Rect, _: &Shared) {
            panic!("composite must not draw full graphs");
        }
        fn layer_depth(&self) -> Option<u8> {
            self.depth
        }
        fn layer(&mut self, _: &mut Ui, _: Rect, _: &Shared) {
            self.drawn.borrow_mut().push(self.title);
        }
    }

    #[test]
    fn composite_draws_layers_bottom_up_by_depth() {
        let ctx = egui::Context::default();
        let drawn = Rc::new(RefCell::new(Vec::new()));
        let mut slots: Vec<Slot> = [
            ("Notes", Some(2)),
            ("Time", None),
            ("Image", Some(0)),
            ("Pitch", Some(1)),
        ]
        .into_iter()
        .map(|(title, depth)| Slot {
            visible: true,
            available: true,
            open: true,
            module: Box::new(Layered {
                title,
                depth,
                drawn: drawn.clone(),
            }),
        })
        .collect();
        let shared = Shared::new(
            Frame {
                samples: Vec::new(),
                rate: 48000,
                start: 0,
            },
            false,
            NoteRange::default(),
            crate::ui::dynamic_range::DbRange::default(),
            Default::default(),
            FftPlans::default(),
        );
        let mut workspace = Workspace {
            view: View::Composite,
            ..Default::default()
        };
        let frame = |workspace: &mut Workspace, slots: &mut [Slot]| {
            drawn.borrow_mut().clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 700.0))),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        workspace.show(ui, slots, &shared, &[], &mut Default::default())
                    });
                },
            );
            output.textures_delta.clear();
            drawn.borrow().clone()
        };
        assert_eq!(
            frame(&mut workspace, &mut slots),
            ["Image", "Pitch", "Notes"]
        );
        // A scrambled (e.g. older persisted) order snaps back to depth order.
        workspace.mix.swap(0, 2);
        workspace.mix[1].on = false;
        assert_eq!(frame(&mut workspace, &mut slots), ["Image", "Notes"]);
        slots[0].visible = false;
        assert_eq!(frame(&mut workspace, &mut slots), ["Image"]);
    }

    /// Stands in for the scope: an own-pane analyser with a full graph.
    struct Pane {
        drawn: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Module for Pane {
        fn title(&self) -> &'static str {
            "Scope"
        }
        fn save_params(&self, _: &mut dyn eframe::Storage, _: &str) {}
        fn load_params(&mut self, _: &dyn eframe::Storage, _: &str) {}
        fn params(&mut self, _: &mut Ui, _: &Shared) {}
        fn graph(&mut self, _: &mut Ui, _: Rect, _: &Shared) {
            self.drawn.borrow_mut().push("Scope");
        }
        fn composite_pane(&self) -> bool {
            true
        }
    }

    #[test]
    fn composite_scope_gets_its_own_pane_below_the_layers() {
        let ctx = egui::Context::default();
        let drawn = Rc::new(RefCell::new(Vec::new()));
        let mut slots: Vec<Slot> = vec![
            Slot {
                visible: true,
                available: true,
                open: true,
                module: Box::new(Layered {
                    title: "Image",
                    depth: Some(0),
                    drawn: drawn.clone(),
                }),
            },
            Slot {
                visible: true,
                available: true,
                open: true,
                module: Box::new(Pane {
                    drawn: drawn.clone(),
                }),
            },
        ];
        let shared = Shared::new(
            Frame {
                samples: Vec::new(),
                rate: 48000,
                start: 0,
            },
            false,
            NoteRange::default(),
            crate::ui::dynamic_range::DbRange::default(),
            Default::default(),
            FftPlans::default(),
        );
        let mut workspace = Workspace {
            view: View::Composite,
            ..Default::default()
        };
        let frame = |workspace: &mut Workspace, slots: &mut [Slot]| {
            drawn.borrow_mut().clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 700.0))),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        workspace.show(ui, slots, &shared, &[], &mut Default::default())
                    });
                },
            );
            output.textures_delta.clear();
            drawn.borrow().clone()
        };
        // Off by default: the scope is listed in the mixer, drawn nowhere.
        assert_eq!(frame(&mut workspace, &mut slots), ["Image"]);
        workspace
            .mix
            .iter_mut()
            .find(|l| l.title == "Scope")
            .unwrap()
            .on = true;
        // Its full graph goes in its own pane, after the stacked layers.
        assert_eq!(frame(&mut workspace, &mut slots), ["Image", "Scope"]);
        // Own-pane analysers sort first in the mix, so the mixer shows them last.
        assert_eq!(
            workspace
                .mix
                .iter()
                .map(|l| l.title.as_str())
                .collect::<Vec<_>>(),
            ["Scope", "Image"]
        );
    }

    #[test]
    fn display_sizes_fill_viewport_and_keep_proportions() {
        assert_eq!(
            proportional_sizes(&[1.0, 2.0, 1.0], 400.0, 72.0),
            [100.0, 200.0, 100.0]
        );
        assert_eq!(
            proportional_sizes(&[1.0, 2.0, 1.0], 800.0, 72.0),
            [200.0, 400.0, 200.0]
        );
        assert_eq!(
            proportional_sizes(&[1.0, 20.0, 1.0], 400.0, 72.0),
            [72.0, 256.0, 72.0]
        );
        assert_eq!(
            proportional_sizes(&[1.0, 20.0, 1.0], 120.0, 72.0),
            [40.0, 40.0, 40.0]
        );
        assert_eq!(proportional_sizes(&[1.0], 400.0, 72.0), [400.0]);
        assert!(proportional_sizes(&[], 400.0, 72.0).is_empty());
    }

    #[test]
    fn solo_page_requests_only_its_analyser_and_host() {
        use crate::modules::{self, note_spectrum, tuner};
        let slots = modules::all();
        let titles = |want: &[bool]| -> Vec<&str> {
            slots
                .iter()
                .zip(want)
                .filter(|(_, w)| **w)
                .map(|(s, _)| s.module.title())
                .collect()
        };
        let waterfall = "Waterfall";
        let mut workspace = Workspace::default();

        // Stack asks for everything.
        assert!(workspace.requested(&slots).iter().all(|&w| w));

        // A plain solo page asks for its own analyser alone.
        workspace.view = View::Solo(waterfall.to_owned());
        assert_eq!(titles(&workspace.requested(&slots)), [waterfall]);

        // The tuner draws over the note spectrum, so both are requested.
        workspace.view = View::Solo(tuner::TITLE.to_owned());
        assert_eq!(
            titles(&workspace.requested(&slots)),
            [tuner::TITLE, note_spectrum::TITLE]
        );

        // Solo on a switched-off analyser falls back to the stack.
        let mut off = modules::all();
        off.iter_mut()
            .find(|s| s.module.title() == waterfall)
            .unwrap()
            .visible = false;
        workspace.view = View::Solo(waterfall.to_owned());
        assert!(workspace.requested(&off).iter().all(|&w| w));
    }
}

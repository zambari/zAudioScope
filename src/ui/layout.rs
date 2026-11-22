//! A scrollable inspector and a display stack that always fits its viewport.

use std::collections::BTreeMap;

use egui::{Align, Align2, FontId, Id, Layout, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use serde::{Deserialize, Serialize};

use super::{noterange, plot, theme, widgets};
use super::outside as noterange_outside;
use crate::modules::{Shared, Slot, tuner};

const DIVIDER: f32 = 8.0;
const HEADER: f32 = 22.0;
const INSPECTOR_DEFAULT: f32 = 300.0;
const INSPECTOR_MIN: f32 = 270.0;
const DISPLAY_MIN: f32 = 72.0;
/// Height reserved for the tab row above the display column.
const TABS: f32 = 24.0;

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
    /// First-time mixer state for an analyzer. The scope starts off at half
    /// opacity so it doesn't swamp the other layers until asked for.
    fn default_for(title: &str) -> Self {
        let (on, opacity) = if title == "Scope" { (false, 0.5) } else { (true, 1.0) };
        Self { title: title.to_owned(), on, opacity }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    inspector_width: f32,
    /// Keyed by title so hiding or adding modules preserves their sizes.
    weights: BTreeMap<String, f32>,
    view: View,
    /// Composite layers, bottom first.
    mix: Vec<MixLayer>,
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
    pub fn show(&mut self, ui: &mut Ui, slots: &mut [Slot], shared: &Shared, marks: &[f32], profiler: &mut super::profiler::Profiler) {
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        let area = Rect::from_min_max(
            full.min,
            pos2(full.right(), (full.bottom() - 8.0 - noterange::BAR_H).max(full.top())),
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
        let visible: Vec<usize> = ordered.iter().copied().filter(|&i| slots[i].visible).collect();
        self.sync_mix(slots);
        let layered = visible.iter().any(|&i| slots[i].module.layer_depth().is_some());
        let merged = merges(slots, &visible);
        let overlays: Vec<(usize, &str)> = merged.iter().map(|&(h, g)| (h, slots[g].module.title())).collect();
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
        let mut tabs: Vec<(Tab, &str, bool)> = vec![(Tab::Stack, "Stack", false)];
        // Every analyzer has a solo tab; a switched-off one is dimmed and
        // clicking it switches it back on.
        tabs.extend(ordered.iter().map(|&i| {
            let title = slots[i].module.title();
            (Tab::Solo(title), title, !slots[i].visible)
        }));
        if layered {
            tabs.push((Tab::Composite, "Composite", false));
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
                && let Some(slot) = slots.iter_mut().find(|s| s.module.title() == title && !s.visible)
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
                        Tab::Composite => slot.module.layer_depth().is_some(),
                        // A solo overlay also shows the graph it draws over.
                        Tab::Solo(solo) => solo == title || overlays.contains(&(i, solo)),
                    };
                    if !shown {
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
                                    ui.push_id("params", |ui| slot.module.params(ui, shared));
                                });
                            });
                            if !slot.visible {
                                ui.painter().set(background, egui::Shape::rect_filled(
                                    content.response.rect.expand(3.0),
                                    0.0,
                                    egui::Color32::from_black_alpha(77),
                                ));
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
                    .filter(|&i| !merged.iter().any(|m| m.1 == i) && !slots[i].module.solo_only())
                    .collect();
                self.displays(ui, displays, slots, &rows, &merged, true, shared, profiler)
            }
            Tab::Solo(title) => {
                let solo: Vec<usize> = visible.into_iter().filter(|&i| slots[i].module.title() == title).collect();
                // An overlay's solo page draws it over its host; a host's is plain.
                match merged.iter().find(|m| solo.contains(&m.1)) {
                    Some(&m) => self.displays(ui, displays, slots, &[m.0], &[m], false, shared, profiler),
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
                vec![pos2(start.x.min(turn_x), y), pos2(turn_x, y), pos2(turn_x, end.y), end]
            } else {
                vec![pos2(turn_x, y), pos2(turn_x, end.y), end]
            };
            let painter = ui.painter().with_clip_rect(area.intersect(ui.clip_rect()));
            let stroke = Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.35));
            // A parameter guide shares this route; pause ours so they don't overlap.
            if !plot::links_pending(ui) {
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
            profiler.record("Range", if reused { "coarse FFT shared" } else { "coarse FFT own" }, start);
            let start = profiler.start();
            let (d, fn_) = (noterange_outside::FINE_DECIMATE, noterange_outside::FINE_FFT);
            let fine = shared.decimated_spectrum(d, fn_);
            profiler.record("Range", "fine FFT /16", start);
            let start = profiler.start();
            let fine_rate = rate as f32 / d as f32;
            self.outside.update(
                &noterange_outside::Source::new(&fine, fine_rate, fn_, fine_rate * noterange_outside::FINE_TOP),
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
        let marks = if matches!(current, Tab::Stack | Tab::Solo(tuner::TITLE)) { marks } else { &[] };
        let outside = self.range_fft.then_some(&self.outside);
        if self.glow.update(&shared.midi) {
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

    /// Keep one order entry per analyzer (new ones go last) and return slot
    /// indices in that order.
    fn sync_order(&mut self, slots: &[Slot]) -> Vec<usize> {
        let index = |title: &str| slots.iter().position(|s| s.module.title() == title);
        let mut seen = std::collections::BTreeSet::new();
        self.order.retain(|t| index(t).is_some() && seen.insert(t.clone()));
        for slot in slots {
            if !self.order.iter().any(|t| t == slot.module.title()) {
                self.order.push(slot.module.title().to_owned());
            }
        }
        self.order.iter().filter_map(|t| index(t)).collect()
    }

    /// Keep one mix entry per layerable analyzer, stacked by depth: the
    /// opaque waterfall at the bottom, then scope trace, spectrum, tuner, and MIDI notes on top.
    fn sync_mix(&mut self, slots: &[Slot]) {
        let depth = |title: &str| {
            slots.iter().find(|s| s.module.title() == title).and_then(|s| s.module.layer_depth())
        };
        self.mix.retain(|l| depth(&l.title).is_some());
        for slot in slots {
            let title = slot.module.title();
            if slot.module.layer_depth().is_some() && !self.mix.iter().any(|l| l.title == title) {
                self.mix.push(MixLayer::default_for(title));
            }
        }
        self.mix.sort_by_key(|l| depth(&l.title));
        for l in &mut self.mix {
            l.opacity = if l.opacity.is_finite() { l.opacity.clamp(0.0, 1.0) } else { 1.0 };
        }
    }

    /// Every layerable analyzer that is on, drawn bottom to top over a shared
    /// note grid in one panel. The Layers button opens the mixer.
    fn composite(&mut self, ui: &mut Ui, area: Rect, slots: &mut [Slot], shared: &Shared, profiler: &mut super::profiler::Profiler) -> BTreeMap<String, Rect> {
        let mut panels = BTreeMap::new();
        ui.painter().rect_filled(area, 2.0, ui.visuals().extreme_bg_color);
        let graph = Rect::from_min_max(pos2(area.left() + 1.0, area.top() + HEADER), area.max - vec2(1.0, 1.0));
        if graph.height() >= 44.0 && graph.width() >= 24.0 {
            plot::note_grid(&ui.painter().with_clip_rect(graph), graph, shared.range.get());
            for layer in self.mix.iter().filter(|l| l.on && l.opacity > 0.0) {
                let Some(slot) = slots.iter_mut().find(|s| s.visible && s.module.title() == layer.title) else { continue };
                let start = profiler.start();
                let mut pane = child(ui, ("composite", slot.module.title()), graph);
                pane.set_opacity(layer.opacity);
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
        // Stays open while the mixer is used; only the button (or Esc) closes it.
        let popup_id = header.scope_id().with("layers_popup");
        let open = egui::Popup::is_id_open(header.ctx(), popup_id);
        let label = egui::RichText::new("Layers ⏷");
        let button = if open {
            egui::Button::new(label)
        } else {
            egui::Button::new(label.color(super::theme::ON_ACCENT)).fill(super::theme::ACCENT)
        };
        let button = header.add(button.small()).on_hover_text("Layer visibility and opacity");
        egui::Popup::menu(&button)
            .id(popup_id)
            .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
            .show(|ui| self.mixer(ui, slots));
        panels
    }

    /// One row per layer, topmost first: on/off and opacity.
    fn mixer(&mut self, ui: &mut Ui, slots: &[Slot]) {
        for layer in self.mix.iter_mut().rev() {
            let on = slots.iter().any(|s| s.visible && s.module.title() == layer.title);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(on, |ui| {
                    ui.add(widgets::flicktoggle(&mut layer.on));
                    ui.add_sized(vec2(84.0, 18.0), egui::Label::new(&layer.title).truncate());
                    ui.add_enabled(
                        layer.on,
                        egui::Slider::new(&mut layer.opacity, 0.0..=1.0)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    );
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
    fn displays(&mut self, ui: &mut Ui, area: Rect, slots: &mut [Slot], rows: &[usize], merged: &[(usize, usize)], stack: bool, shared: &Shared, profiler: &mut super::profiler::Profiler) -> BTreeMap<String, Rect> {
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
        let expanded: Vec<usize> = (0..rows.len()).filter(|&r| slots[rows[r]].visible).collect();
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
        for (&r, size) in expanded.iter().zip(proportional_sizes(&weights, available, DISPLAY_MIN)) {
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
            pane.painter()
                .rect_filled(rect, 2.0, if slot.visible { fill } else { fill.gamma_multiply(0.5) });
            // At the minimum window height, use an overlaid title so the
            // axes still have room even with every analyzer visible.
            let header = if !slot.visible || height >= DISPLAY_MIN { HEADER } else { 0.0 };
            let graph = Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.top() + header),
                rect.max - vec2(1.0, 1.0),
            );
            // Tiny windows still fit; don't pass inverted plot bounds to modules.
            if slot.visible && graph.height() >= 44.0 && graph.width() >= 24.0 {
                slot.module.graph(&mut pane, graph, shared);
                if let Some(g) = guest {
                    slots[g].module.overlay(&mut pane, graph, shared);
                }
            }
            let slot = &mut slots[i];
            let name = match guest_title {
                Some(g) => format!("{title} + {g}"),
                None => title.to_owned(),
            };
            let label = if stack {
                format!("{}  {name}", if slot.visible { "⏷" } else { "⏵" })
            } else {
                name
            };
            let color = ui.visuals().weak_text_color();
            let strip = if header > 0.0 {
                let strip = rect.with_max_y(rect.top() + HEADER);
                let font = FontId::proportional(11.0);
                let name = pane.painter().text(strip.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, label, font, color);
                (strip, name.expand2(vec2(6.0, 4.0)))
            } else {
                let tag = plot::tag(pane.painter(), rect, rect.right_top() + vec2(-6.0, 2.0), Align2::RIGHT_TOP, label, color);
                (tag, tag)
            };
            if stack {
                let (strip, name) = strip;
                let response = pane
                    .interact(strip, pane.scope_id().with("panel_header"), Sense::click_and_drag())
                    .on_hover_text("Click the arrow to fold (switches the analyzer off) · drag to reorder");
                let on_name = |pos: Option<egui::Pos2>| pos.is_some_and(|p| name.contains(p));
                if response.hovered() && on_name(response.hover_pos()) {
                    pane.painter().rect_filled(name, 2.0, ui.visuals().widgets.hovered.weak_bg_fill.gamma_multiply(0.6));
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
                None => others.last().map_or(rects[from].bottom(), |&r| rects[r].bottom()) + DIVIDER * 0.5,
            };
            let painter = ui.painter();
            painter.rect_stroke(rects[from], 2.0, Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.6)), egui::StrokeKind::Inside);
            painter.hline(area.x_range(), line_y, Stroke::new(2.0, theme::ACCENT_BRIGHT));
            if released {
                let moved = slots[rows[from]].module.title().to_owned();
                // Positions in `self.order` of the rows shown, so a solo page
                // or a missing slot never scrambles the rest.
                let mut titles: Vec<String> = rows.iter().map(|&i| slots[i].module.title().to_owned()).collect();
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
            let h = visible.iter().copied().find(|&h| h != g && slots[h].module.title() == host)?;
            Some((h, g))
        })
        .collect()
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
                    egui::CentralPanel::default()
                        .show(ui, |ui| workspace.show(ui, slots, &shared, &[], &mut Default::default()));
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
        frame(&mut workspace, &mut slots, vec![egui::Event::PointerMoved(grab), pointer(grab, true)]);
        for step in 1..=4 {
            let at = grab + (drop - grab) * (step as f32 / 4.0);
            frame(&mut workspace, &mut slots, vec![egui::Event::PointerMoved(at)]);
        }
        frame(&mut workspace, &mut slots, vec![pointer(drop, false)]);
        assert_eq!(workspace.order, ["Two", "Three", "One"]);

        // Clicking the top header's name folds it: off, inspector closed.
        let top = frame(&mut workspace, &mut slots, vec![]);
        let name = pos2(top[0].left() + 24.0, top[0].top() - HEADER * 0.5);
        frame(&mut workspace, &mut slots, vec![egui::Event::PointerMoved(name), pointer(name, true)]);
        frame(&mut workspace, &mut slots, vec![pointer(name, false)]);
        assert!(!slots[1].visible && !slots[1].open);
        let folded = frame(&mut workspace, &mut slots, vec![]);
        assert_eq!(folded.len(), 2);
        // The folded strip stays on top, so the remaining panels start below it.
        assert!(folded[0].top() > top[0].top() + DIVIDER);
        frame(&mut workspace, &mut slots, vec![egui::Event::PointerMoved(name), pointer(name, true)]);
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
        let mut slots: Vec<Slot> = [("Notes", Some(2)), ("Time", None), ("Image", Some(0)), ("Pitch", Some(1))]
            .into_iter()
            .map(|(title, depth)| Slot {
                visible: true,
                open: true,
                module: Box::new(Layered { title, depth, drawn: drawn.clone() }),
            })
            .collect();
        let shared = Shared::new(
            Frame { samples: Vec::new(), rate: 48000, start: 0 },
            false,
            NoteRange::default(),
            crate::ui::dynamic_range::DbRange::default(),
            Default::default(),
            FftPlans::default(),
        );
        let mut workspace = Workspace { view: View::Composite, ..Default::default() };
        let frame = |workspace: &mut Workspace, slots: &mut [Slot]| {
            drawn.borrow_mut().clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 700.0))),
                    ..Default::default()
                },
                |ui| {
                    egui::CentralPanel::default()
                        .show(ui, |ui| workspace.show(ui, slots, &shared, &[], &mut Default::default()));
                },
            );
            output.textures_delta.clear();
            drawn.borrow().clone()
        };
        assert_eq!(frame(&mut workspace, &mut slots), ["Image", "Pitch", "Notes"]);
        // A scrambled (e.g. older persisted) order snaps back to depth order.
        workspace.mix.swap(0, 2);
        workspace.mix[1].on = false;
        assert_eq!(frame(&mut workspace, &mut slots), ["Image", "Notes"]);
        slots[0].visible = false;
        assert_eq!(frame(&mut workspace, &mut slots), ["Image"]);
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
}

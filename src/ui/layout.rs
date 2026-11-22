//! A scrollable inspector and a display stack that always fits its viewport.

use std::collections::BTreeMap;

use egui::{Align, Align2, FontId, Id, Layout, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use serde::{Deserialize, Serialize};

use super::{noterange, plot, theme, widgets};
use crate::modules::{Shared, Slot};

const DIVIDER: f32 = 8.0;
const HEADER: f32 = 22.0;
const INSPECTOR_DEFAULT: f32 = 300.0;
const INSPECTOR_MIN: f32 = 270.0;
const DISPLAY_MIN: f32 = 72.0;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    inspector_width: f32,
    /// Keyed by title so hiding or adding modules preserves their sizes.
    weights: BTreeMap<String, f32>,
    /// Interaction feedback is transient; geometry is collected on every repaint.
    #[serde(skip)]
    touched: Option<String>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            inspector_width: INSPECTOR_DEFAULT,
            weights: BTreeMap::new(),
            touched: None,
        }
    }
}

impl Workspace {
    pub fn show(&mut self, ui: &mut Ui, slots: &mut [Slot], shared: &Shared, marks: &[f32], profiler: &mut super::profiler::Profiler) {
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        let area = Rect::from_min_max(
            full.min,
            pos2(full.right(), (full.bottom() - 44.0).max(full.top())),
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
        let displays = Rect::from_min_max(pos2(inspector.right() + DIVIDER, area.top()), area.max);
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
                for slot in slots.iter_mut() {
                    let start = profiler.start();
                    let title = slot.module.title();
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
                    if pressed_in(ui, section.response.rect) {
                        self.touched = Some(title.to_owned());
                    }
                }
            });
        let panels = self.displays(ui, displays, slots, shared, profiler);
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
            painter.vline(panel.left(), panel.y_range(), Stroke::new(2.0, theme::ACCENT_BRIGHT));
        }
        // Draw after both columns so graphs cannot cover a cross-panel guide.
        plot::paint_links(ui, area);
        let range_rect = Rect::from_min_max(pos2(full.left(), area.bottom() + 8.0), full.max);
        let mut range_ui = child(ui, "note_range", range_rect);
        let mut range = shared.range.get();
        let readout_width = self.inspector_width + DIVIDER - range_ui.spacing().item_spacing.x;
        if noterange::bar(&mut range_ui, &mut range, marks, readout_width) {
            shared.range.set(range);
        }
    }

    fn displays(&mut self, ui: &mut Ui, area: Rect, slots: &mut [Slot], shared: &Shared, profiler: &mut super::profiler::Profiler) -> BTreeMap<String, Rect> {
        let mut panels = BTreeMap::new();
        let visible: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.visible)
            .map(|(i, _)| i)
            .collect();
        if visible.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                "Turn on an analyzer to show its display",
                FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return panels;
        }
        let weights: Vec<f32> = visible
            .iter()
            .map(|&i| {
                let module = &slots[i].module;
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
        let available = (area.height() - DIVIDER * (visible.len() - 1) as f32).max(0.0);
        let mut heights = proportional_sizes(&weights, available, DISPLAY_MIN);
        let minimum = DISPLAY_MIN.min(available / visible.len() as f32);
        let mut y = area.top();
        let mut resized = false;
        for row in 0..visible.len() - 1 {
            y += heights[row];
            let rect = Rect::from_min_size(pos2(area.left(), y), vec2(area.width(), DIVIDER));
            let id = ui.scope_id().with((
                "display_divider",
                slots[visible[row]].module.title(),
                slots[visible[row + 1]].module.title(),
            ));
            let response = divider(ui, id, rect, false);
            if response.dragged() || response.double_clicked() {
                let delta = if response.double_clicked() {
                    (heights[row + 1] - heights[row]) * 0.5
                } else {
                    response.drag_delta().y
                };
                let delta = delta.clamp(
                    (minimum - heights[row]).min(0.0),
                    (heights[row + 1] - minimum).max(0.0),
                );
                heights[row] += delta;
                heights[row + 1] -= delta;
                y += delta;
                resized = true;
            }
            y += DIVIDER;
        }
        if resized && available > 0.0 {
            let total: f32 = weights.iter().sum();
            for (&i, &height) in visible.iter().zip(&heights) {
                self.weights.insert(
                    slots[i].module.title().to_owned(),
                    height / available * total,
                );
            }
        }
        y = area.top();
        for (&i, height) in visible.iter().zip(heights) {
            let start = profiler.start();
            let slot = &mut slots[i];
            let rect = Rect::from_min_size(pos2(area.left(), y), vec2(area.width(), height));
            panels.insert(slot.module.title().to_owned(), rect);
            let mut pane = child(ui, slot.module.title(), rect);
            pane.painter()
                .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
            // At the minimum window height, use an overlaid title so the
            // axes still have room even with every analyzer visible.
            let header = if height >= DISPLAY_MIN { HEADER } else { 0.0 };
            let graph = Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.top() + header),
                rect.max - vec2(1.0, 1.0),
            );
            // Tiny windows still fit; don't pass inverted plot bounds to modules.
            if graph.height() >= 44.0 && graph.width() >= 24.0 {
                slot.module.graph(&mut pane, graph, shared);
            }
            if header > 0.0 {
                pane.painter().text(
                    rect.left_top() + vec2(8.0, HEADER * 0.5),
                    Align2::LEFT_CENTER,
                    slot.module.title(),
                    FontId::proportional(11.0),
                    ui.visuals().weak_text_color(),
                );
            } else {
                plot::tag(
                    pane.painter(),
                    rect,
                    rect.right_top() + vec2(-6.0, 2.0),
                    Align2::RIGHT_TOP,
                    slot.module.title(),
                    ui.visuals().weak_text_color(),
                );
            }
            if pressed_in(ui, rect) {
                self.touched = Some(slot.module.title().to_owned());
            }
            profiler.record("Display", slot.module.title(), start);
            y += height + DIVIDER;
        }
        panels
    }
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
        for slot in &mut slots {
            slot.visible = false;
        }
        assert!(frame(&mut workspace, &mut slots, vec![]).is_empty());
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

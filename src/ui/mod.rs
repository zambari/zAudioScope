pub mod handle;
pub mod infobox;
pub mod meter;
pub mod noterange;
pub mod notestyle;
pub mod plot;
pub mod theme;
pub mod widgets;

use egui::{Color32, Sense, Stroke, Ui};

use crate::modules::{Module, Shared};

/// Width of the parameter column on the left of every analyser row.
pub const PARAMS_W: f32 = 230.0;

/// A foldout analyser row: a header that opens/closes it, then a fixed-width
/// parameter column and a graph taking the rest of the width.
pub fn module_row(ui: &mut Ui, module: &mut dyn Module, open: &mut bool, shared: &Shared) {
    let (title, graph_h) = (module.title(), module.graph_height());
    let arrow = if *open { "⏷" } else { "⏵" };
    let header = ui.add(
        egui::Button::new(egui::RichText::new(format!("{arrow}  {title}")).strong())
            .frame(false)
            .min_size(egui::vec2(ui.available_width(), 20.0)),
    );
    if header.clicked() {
        *open = !*open;
    }
    if !*open {
        ui.separator();
        return;
    }
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(PARAMS_W, graph_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(PARAMS_W);
                ui.set_min_height(graph_h);
                ui.push_id("params", |ui| module.params(ui, shared));
            },
        );
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), graph_h), Sense::hover());
        ui.painter().rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        module.graph(ui, rect, shared);
        ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::from_gray(48)), egui::StrokeKind::Inside);
    });
    ui.separator();
}

//! Edge handles: a small reddish triangle glued to one side of a graph that
//! drags a value along that side (the scope's threshold on its left edge,
//! the waterfall's frequency zoom along its bottom...). The tip touches the
//! edge at the value. With the pointer elsewhere it shrinks to a dot;
//! hovering that side of the graph brings it back, with its label.
//!
//! Register handles after any whole-graph interaction (like the scope's
//! drag-to-select) so they win the hit test.

use std::hash::Hash;

use egui::{Align2, Color32, CursorIcon, Id, Rect, Response, Sense, Shape, Stroke, Ui, vec2};

use super::plot::{self, Edge, Scale};
use super::theme;

/// Depth of the strip along an edge that reveals its handles.
pub const ZONE: f32 = 34.0;
/// Triangle length, tip to base.
const SIZE: f32 = 9.0;

/// 0..1, eased: how revealed the handles on `edge` should be. Full while
/// the pointer is in the strip along that side, or when `force`d.
pub fn reveal(ui: &Ui, id: Id, rect: Rect, edge: Edge, force: bool) -> f32 {
    let zone = match edge {
        Edge::Left => Rect::from_x_y_ranges(rect.left() - 8.0..=rect.left() + ZONE, rect.y_range()),
        Edge::Right => Rect::from_x_y_ranges(rect.right() - ZONE..=rect.right() + 8.0, rect.y_range()),
        Edge::Top => Rect::from_x_y_ranges(rect.x_range(), rect.top() - 8.0..=rect.top() + ZONE),
        Edge::Bottom => Rect::from_x_y_ranges(rect.x_range(), rect.bottom() - ZONE..=rect.bottom() + 8.0),
    };
    let near = force || ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| zone.contains(p));
    ui.ctx().animate_bool_with_time(id.with("reveal"), near, 0.18)
}

pub struct EdgeHandle<'a> {
    salt: Id,
    edge: Edge,
    scale: Scale,
    value: &'a mut f32,
    clamp: Option<(f32, f32)>,
    label: Option<&'a dyn Fn(f32) -> String>,
    color: Color32,
    guide: bool,
    reset: Option<f32>,
}

impl<'a> EdgeHandle<'a> {
    /// `scale` maps the value onto pixels along `edge` (y for left/right).
    pub fn new(id_salt: impl Hash + std::fmt::Debug, edge: Edge, scale: Scale, value: &'a mut f32) -> Self {
        Self { salt: Id::NULL.with(id_salt), edge, scale, value, clamp: None, label: None, color: theme::HANDLE, guide: true, reset: None }
    }

    /// Allowed values; defaults to the scale's range.
    pub fn clamp(mut self, lo: f32, hi: f32) -> Self {
        self.clamp = Some((lo.min(hi), lo.max(hi)));
        self
    }

    /// Text shown next to the handle when revealed.
    pub fn label(mut self, fmt: &'a dyn Fn(f32) -> String) -> Self {
        self.label = Some(fmt);
        self
    }

    #[allow(dead_code)] // toolkit
    pub fn color(mut self, c: Color32) -> Self {
        self.color = c;
        self
    }

    /// Faint line across the graph while hovered or dragged (default on).
    pub fn guide(mut self, on: bool) -> Self {
        self.guide = on;
        self
    }

    /// Double-click sets this value.
    pub fn reset(mut self, v: f32) -> Self {
        self.reset = Some(v);
        self
    }

    pub fn show(self, ui: &mut Ui, rect: Rect) -> Response {
        let id = ui.scope_id().with(self.salt);
        let (edge, scale) = (self.edge, self.scale);
        let span = edge.span(rect);
        let n = edge.inward();
        let tip_at = |v: f32| edge.point(rect, scale.px(v).clamp(span.min, span.max));

        let hit = Rect::from_center_size(tip_at(*self.value) + n * (SIZE * 0.5), vec2(SIZE + 8.0, SIZE + 8.0));
        let cursor = if edge.vertical() { CursorIcon::ResizeVertical } else { CursorIcon::ResizeHorizontal };
        let mut resp = ui.interact(hit, id, Sense::click_and_drag()).on_hover_cursor(cursor);
        let (lo, hi) = self.clamp.unwrap_or_else(|| scale.range());
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let v = scale.value(edge.along(p)).clamp(lo, hi);
            if v != *self.value {
                *self.value = v;
                resp.mark_changed();
            }
        }
        if resp.double_clicked()
            && let Some(d) = self.reset
        {
            *self.value = d.clamp(lo, hi);
            resp.mark_changed();
        }

        let active = resp.hovered() || resp.dragged();
        let vis = reveal(ui, id, rect, edge, active);
        let tip = tip_at(*self.value);
        let painter = ui.painter_at(rect);
        let color = if active { self.color } else { self.color.gamma_multiply(0.8) };

        if self.guide && active {
            let s = Stroke::new(1.0, self.color.gamma_multiply(if resp.dragged() { 0.5 } else { 0.3 }));
            let along = edge.along(tip);
            if edge.vertical() {
                painter.hline(rect.x_range(), along, s);
            } else {
                painter.vline(along, rect.y_range(), s);
            }
        }
        if vis < 0.99 {
            painter.circle_filled(tip + n * 3.0, 2.0, self.color.gamma_multiply(0.65 * (1.0 - vis)));
        }
        if vis > 0.01 {
            let s = SIZE * (0.45 + 0.55 * vis);
            let side = vec2(-n.y, n.x) * (s * 0.6);
            let base = tip + n * s;
            painter.add(Shape::convex_polygon(vec![tip, base + side, base - side], color.gamma_multiply(vis), Stroke::NONE));
        }
        if let Some(fmt) = self.label
            && vis > 0.02
        {
            let anchor = match edge {
                Edge::Left => Align2::LEFT_CENTER,
                Edge::Right => Align2::RIGHT_CENTER,
                Edge::Top => Align2::CENTER_TOP,
                Edge::Bottom => Align2::CENTER_BOTTOM,
            };
            let a = vis * if active { 1.0 } else { 0.75 };
            let c = Color32::from_rgb(255, 214, 205).gamma_multiply(a);
            plot::tag(&painter, rect.shrink(1.0), tip + n * (SIZE + 3.0), anchor, fmt(*self.value), c);
        }
        resp
    }
}

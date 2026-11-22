//! Shared graph furniture, so every analyser labels its axes the same way:
//!
//! - [`Scale`]: value ↔ pixel along one axis (linear, log, or a custom warp
//!   such as a knee-warped frequency axis), with "nice" tick values.
//! - [`Axis`]: faint technical tick marks glued to one edge of the graph,
//!   optional grid lines, and small monospace labels inside the plot.
//! - [`Lanes`]: stacks labels into rows so neighbours don't overlap.
//! - [`caption`] / [`tag`]: corner readouts and backed labels.
//! - [`Link`]: ties a parameter slider to the spot on the graph it
//!   controls; while the slider is hovered or dragged a dashed leader runs
//!   from it to a highlighted marker on the graph.

use egui::{
    Align2, Color32, FontId, Painter, Pos2, Rangef, Rect, Response, Shape, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};

use super::theme;

/// Grid line behind a labelled tick.
pub const GRID: Color32 = Color32::from_rgba_premultiplied(11, 11, 11, 11);
/// Grid line behind a major (decade / round) tick.
pub const GRID_MAJOR: Color32 = Color32::from_rgba_premultiplied(22, 22, 22, 22);
/// Tick marks on the edge.
pub const TICK: Color32 = Color32::from_rgba_premultiplied(70, 70, 70, 70);
/// Axis label text.
pub const LABEL: Color32 = Color32::from_rgba_premultiplied(95, 95, 95, 95);

pub fn font() -> FontId {
    FontId::monospace(9.5)
}

/// One side of a graph rect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Left,
    Right,
    #[allow(dead_code)] // toolkit: no graph uses a top-edge axis yet
    Top,
    Bottom,
}

impl Edge {
    /// Runs top to bottom, so its values sit along y.
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    /// Unit vector from this edge into the rect.
    pub fn inward(self) -> Vec2 {
        match self {
            Edge::Left => vec2(1.0, 0.0),
            Edge::Right => vec2(-1.0, 0.0),
            Edge::Top => vec2(0.0, 1.0),
            Edge::Bottom => vec2(0.0, -1.0),
        }
    }

    /// Pixel extent along this edge.
    pub fn span(self, rect: Rect) -> Rangef {
        if self.vertical() {
            rect.y_range()
        } else {
            rect.x_range()
        }
    }

    /// The point on this edge at `along` (y for left/right, x for top/bottom).
    pub fn point(self, rect: Rect, along: f32) -> Pos2 {
        match self {
            Edge::Left => pos2(rect.left(), along),
            Edge::Right => pos2(rect.right(), along),
            Edge::Top => pos2(along, rect.top()),
            Edge::Bottom => pos2(along, rect.bottom()),
        }
    }

    /// The coordinate of `p` along this edge.
    pub fn along(self, p: Pos2) -> f32 {
        if self.vertical() { p.y } else { p.x }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Linear,
    Log,
    /// Monotonic forward / inverse warp; linear in warped units.
    #[allow(dead_code)] // toolkit
    Warp(fn(f32) -> f32, fn(f32) -> f32),
}

impl Kind {
    fn fwd(self, v: f32) -> f32 {
        match self {
            Kind::Linear => v,
            Kind::Log => v.max(1e-12).ln(),
            Kind::Warp(f, _) => f(v),
        }
    }

    fn inv(self, w: f32) -> f32 {
        match self {
            Kind::Linear => w,
            Kind::Log => w.exp(),
            Kind::Warp(_, i) => i(w),
        }
    }
}

/// A tick value; major ones get a longer mark and a stronger grid line.
#[derive(Clone, Copy, Debug)]
pub struct Tick {
    pub value: f32,
    pub major: bool,
}

/// Maps values onto pixels along one axis: `values.0` lands on `px.0`,
/// `values.1` on `px.1` (so a y axis with "up is more" passes
/// `(bottom, top)`). Extrapolates outside the range.
#[derive(Clone, Copy)]
pub struct Scale {
    kind: Kind,
    lo: f32,
    hi: f32,
    w_lo: f32,
    w_hi: f32,
    p_lo: f32,
    p_hi: f32,
}

impl Scale {
    fn new(kind: Kind, values: (f32, f32), px: (f32, f32)) -> Self {
        Self {
            kind,
            lo: values.0,
            hi: values.1,
            w_lo: kind.fwd(values.0),
            w_hi: kind.fwd(values.1),
            p_lo: px.0,
            p_hi: px.1,
        }
    }

    pub fn linear(values: (f32, f32), px: (f32, f32)) -> Self {
        Self::new(Kind::Linear, values, px)
    }

    /// Values must be positive.
    pub fn log(values: (f32, f32), px: (f32, f32)) -> Self {
        Self::new(Kind::Log, values, px)
    }

    /// Linear in `fwd(value)`; `inv` undoes `fwd`. Ticks are picked like a
    /// log axis (1-2-5 per decade, thinned to fit).
    #[allow(dead_code)] // toolkit
    pub fn warped(
        fwd: fn(f32) -> f32,
        inv: fn(f32) -> f32,
        values: (f32, f32),
        px: (f32, f32),
    ) -> Self {
        Self::new(Kind::Warp(fwd, inv), values, px)
    }

    pub fn px(&self, v: f32) -> f32 {
        let d = self.w_hi - self.w_lo;
        if d == 0.0 {
            return self.p_lo;
        }
        self.p_lo + (self.kind.fwd(v) - self.w_lo) / d * (self.p_hi - self.p_lo)
    }

    pub fn value(&self, px: f32) -> f32 {
        let d = self.p_hi - self.p_lo;
        if d == 0.0 {
            return self.lo;
        }
        self.kind
            .inv(self.w_lo + (px - self.p_lo) / d * (self.w_hi - self.w_lo))
    }

    /// (smallest, largest) value.
    pub fn range(&self) -> (f32, f32) {
        (self.lo.min(self.hi), self.lo.max(self.hi))
    }

    /// Tick values at least `min_px` apart.
    pub fn ticks(&self, min_px: f32) -> Vec<Tick> {
        let px = (self.p_hi - self.p_lo).abs();
        let (a, b) = self.range();
        if !(b > a) || px < 1.0 || min_px <= 0.0 {
            return Vec::new();
        }
        match self.kind {
            Kind::Linear => linear_ticks(a, b, px, min_px),
            Kind::Log | Kind::Warp(..) => self.decade_ticks(a, b, min_px),
        }
    }

    /// 1, 5, 2, 3, 4… per decade in priority order, kept when at least
    /// `min_px` from everything kept so far.
    fn decade_ticks(&self, a: f32, b: f32, min_px: f32) -> Vec<Tick> {
        const STEPS: [(f32, u8); 9] = [
            (1.0, 0),
            (5.0, 1),
            (2.0, 2),
            (3.0, 3),
            (4.0, 4),
            (6.0, 4),
            (8.0, 4),
            (7.0, 5),
            (9.0, 5),
        ];
        let a = a.max(1e-6);
        let mut cands = Vec::new();
        for e in a.log10().floor() as i32..=b.log10().ceil() as i32 {
            let d = 10f32.powi(e);
            for (m, prio) in STEPS {
                let v = m * d;
                if v >= a * 0.9999 && v <= b * 1.0001 {
                    cands.push((prio, v));
                }
            }
        }
        cands.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)));
        let mut kept: Vec<(f32, Tick)> = Vec::new();
        for (prio, v) in cands {
            let p = self.px(v);
            if kept.iter().all(|(q, _)| (q - p).abs() >= min_px) {
                kept.push((
                    p,
                    Tick {
                        value: v,
                        major: prio == 0,
                    },
                ));
            }
        }
        let mut out: Vec<Tick> = kept.into_iter().map(|k| k.1).collect();
        out.sort_by(|x, y| x.value.total_cmp(&y.value));
        out
    }
}

/// Steps of 1, 2 or 5 × 10ⁿ; majors at round multiples.
fn linear_ticks(a: f32, b: f32, px: f32, min_px: f32) -> Vec<Tick> {
    let raw = (b - a) / (px / min_px).max(1.0);
    let mag = 10f32.powf(raw.log10().floor());
    let (m, every) = [(1.0, 5.0), (2.0, 5.0), (5.0, 2.0), (10.0, 5.0)]
        .into_iter()
        .find(|(m, _)| m * mag >= raw)
        .unwrap_or((10.0, 5.0));
    let step = m * mag;
    let major = step * every;
    let (k0, k1) = (
        (a / step - 1e-4).ceil() as i64,
        (b / step + 1e-4).floor() as i64,
    );
    (k0..=k1)
        .map(|k| {
            let v = k as f32 * step;
            Tick {
                value: if v.abs() < step * 1e-4 { 0.0 } else { v },
                major: ((v / major).round() * major - v).abs() < step * 1e-3,
            }
        })
        .collect()
}

/// Ticks and labels along one edge of a graph.
pub struct Axis {
    edge: Edge,
    scale: Scale,
    spacing: f32,
    grid: bool,
    minor: bool,
}

impl Axis {
    pub fn new(edge: Edge, scale: Scale) -> Self {
        Self {
            edge,
            scale,
            spacing: if edge.vertical() { 30.0 } else { 64.0 },
            grid: true,
            minor: true,
        }
    }

    /// Least pixels between labelled ticks.
    pub fn spacing(mut self, px: f32) -> Self {
        self.spacing = px;
        self
    }

    /// Faint grid lines across the graph at labelled ticks.
    pub fn grid(mut self, on: bool) -> Self {
        self.grid = on;
        self
    }

    /// Unlabelled small marks between labelled ticks.
    pub fn minor(mut self, on: bool) -> Self {
        self.minor = on;
        self
    }

    /// Draw inside `rect`; `fmt` turns a tick value into its label (an
    /// empty string draws the mark only).
    pub fn draw(&self, painter: &Painter, rect: Rect, fmt: impl Fn(f32) -> String) {
        let span = self.edge.span(rect);
        let n = self.edge.inward();
        let inside = |p: f32| p >= span.min - 0.5 && p <= span.max + 0.5;
        if self.minor {
            for t in self.scale.ticks(self.spacing / 5.0) {
                let p = self.scale.px(t.value);
                if inside(p) {
                    let a = self.edge.point(rect, p);
                    painter.line_segment(
                        [a, a + n * 2.0],
                        Stroke::new(1.0, TICK.gamma_multiply(0.45)),
                    );
                }
            }
        }
        for t in self.scale.ticks(self.spacing) {
            let p = self.scale.px(t.value);
            if !inside(p) {
                continue;
            }
            if self.grid {
                let s = Stroke::new(1.0, if t.major { GRID_MAJOR } else { GRID });
                if self.edge.vertical() {
                    painter.hline(rect.x_range(), p, s);
                } else {
                    painter.vline(p, rect.y_range(), s);
                }
            }
            let a = self.edge.point(rect, p);
            painter.line_segment(
                [a, a + n * if t.major { 5.0 } else { 3.5 }],
                Stroke::new(1.0, TICK),
            );
            let text = fmt(t.value);
            if text.is_empty() {
                continue;
            }
            let g = painter.layout_no_wrap(text, font(), LABEL);
            let s = g.size();
            let pos = match self.edge {
                Edge::Bottom | Edge::Top => {
                    let y = if self.edge == Edge::Bottom {
                        rect.bottom() - 2.0 - s.y
                    } else {
                        rect.top() + 2.0
                    };
                    let x = if p + 2.0 + s.x > rect.right() - 2.0 {
                        p - 2.0 - s.x
                    } else {
                        p + 2.0
                    };
                    pos2(x, y)
                }
                Edge::Left | Edge::Right => {
                    let x = if self.edge == Edge::Left {
                        rect.left() + 7.0
                    } else {
                        rect.right() - 7.0 - s.x
                    };
                    let y = if p - s.y < rect.top() + 1.0 {
                        p + 1.0
                    } else {
                        p - s.y
                    };
                    pos2(x, y)
                }
            };
            painter.galley(pos, g, LABEL);
        }
    }
}

/// Assigns labels, placed left to right, to rows so they don't overlap.
pub struct Lanes {
    ends: Vec<f32>,
    gap: f32,
}

impl Lanes {
    pub fn new(lanes: usize, gap: f32) -> Self {
        Self {
            ends: vec![f32::MIN; lanes.max(1)],
            gap,
        }
    }

    /// Lane for a label spanning `left..left + width`: the first one it fits
    /// in, otherwise the last.
    pub fn place(&mut self, left: f32, width: f32) -> usize {
        let i = self
            .ends
            .iter()
            .position(|&e| left > e + self.gap)
            .unwrap_or(self.ends.len() - 1);
        self.ends[i] = left + width;
        i
    }
}

#[allow(dead_code)] // toolkit
/// Lay out `text`, anchor it at `pos`, nudge it inside `bounds` and draw it.
/// Returns where it went.
pub fn text_in(
    painter: &Painter,
    bounds: Rect,
    pos: Pos2,
    anchor: Align2,
    text: impl ToString,
    font: FontId,
    color: Color32,
) -> Rect {
    let g = painter.layout_no_wrap(text.to_string(), font, color);
    let r = nudge_inside(anchor.anchor_size(pos, g.size()), bounds);
    painter.galley(r.min, g, color);
    r
}

/// Like [`text_in`] on a dark backing, for labels over busy graphs.
pub fn tag(
    painter: &Painter,
    bounds: Rect,
    pos: Pos2,
    anchor: Align2,
    text: impl ToString,
    color: Color32,
) -> Rect {
    let g = painter.layout_no_wrap(text.to_string(), font(), color);
    let pad = vec2(3.0, 1.0);
    let r = nudge_inside(anchor.anchor_size(pos, g.size() + 2.0 * pad), bounds);
    painter.rect_filled(
        r,
        2.0,
        Color32::from_black_alpha((170.0 * color.a() as f32 / 255.0) as u8),
    );
    painter.galley(r.min + pad, g, color);
    r
}

/// A readout in one corner of a graph ("600 ms after trigger").
pub fn caption(painter: &Painter, rect: Rect, corner: Align2, text: impl ToString) {
    let inset = vec2(4.0, 3.0);
    let pos = pos2(
        match corner.x() {
            egui::Align::Min => rect.left() + inset.x,
            egui::Align::Center => rect.center().x,
            egui::Align::Max => rect.right() - inset.x,
        },
        match corner.y() {
            egui::Align::Min => rect.top() + inset.y,
            egui::Align::Center => rect.center().y,
            egui::Align::Max => rect.bottom() - inset.y,
        },
    );
    tag(painter, rect, pos, corner, text, Color32::from_gray(150));
}

fn nudge_inside(r: Rect, bounds: Rect) -> Rect {
    let dx = (bounds.left() - r.left()).max(0.0) - (r.right() - bounds.right()).max(0.0);
    let dy = (bounds.top() - r.top()).max(0.0) - (r.bottom() - bounds.bottom()).max(0.0);
    r.translate(vec2(dx, dy))
}

/// "0.25", "3", "-1.5": up to two decimals, trailing zeros dropped.
pub fn trim(v: f32) -> String {
    let v = if v.abs() < 1e-6 { 0.0 } else { v };
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// "250ms", "1.5s".
pub fn fmt_ms(ms: f32) -> String {
    if ms.abs() >= 1000.0 {
        format!("{}s", trim(ms / 1000.0))
    } else {
        format!("{}ms", trim(ms))
    }
}

/// "0.5s".
pub fn fmt_secs(s: f32) -> String {
    format!("{}s", trim(s))
}

/// "50", "1.2k".
pub fn fmt_hz(f: f32) -> String {
    if f >= 1000.0 {
        format!("{}k", trim(f / 1000.0))
    } else {
        trim(f)
    }
}

/// Ties parameter sliders to points on the graph. Keep one per module;
/// call [`Link::slider`] in `params`, then [`Link::tick`] at the start of
/// `graph` and [`Link::point_x`] / [`Link::point_y`] for whichever key
/// [`Link::shown`] returns.
pub struct Link<K> {
    sliders: Vec<(K, Rect, u64)>,
    hot: Option<K>,
    shown: Option<K>,
    alpha: f32,
}

impl<K> Default for Link<K> {
    fn default() -> Self {
        Self {
            sliders: Vec::new(),
            hot: None,
            shown: None,
            alpha: 0.0,
        }
    }
}

impl<K: Copy + PartialEq> Link<K> {
    /// Add `slider`; hovering or dragging it points at its spot on the graph.
    /// Outlined while its graph handle is in use.
    pub fn slider(&mut self, ui: &mut Ui, key: K, slider: egui::Slider<'_>) -> Response {
        let r = ui.add(slider);
        let frame = ui.ctx().cumulative_frame_nr();
        // Only register the visible part; old anchors must not survive a
        // collapsed foldout or a slider scrolled out of the inspector.
        let visible = r.rect.intersect(ui.clip_rect());
        self.sliders.retain(|s| s.0 != key);
        if visible.is_positive() {
            self.sliders.push((key, visible, frame));
        }
        if r.hovered() || r.dragged() || r.has_focus() {
            self.hot = Some(key);
        }
        if self.shown == Some(key) && self.alpha > 0.02 {
            let s = Stroke::new(1.0, theme::ACCENT_BRIGHT.gamma_multiply(0.45 * self.alpha));
            ui.painter()
                .rect_stroke(r.rect.expand(1.0), 2.0, s, StrokeKind::Outside);
        }
        r
    }

    /// Mark `key` as in use from the graph side (its handle is hovered).
    pub fn touch(&mut self, key: K) {
        self.hot = Some(key);
    }

    /// Once per frame before drawing pointers.
    pub fn tick(&mut self, dt: f32) {
        let target = match self.hot.take() {
            Some(k) => {
                self.shown = Some(k);
                1.0
            }
            None => 0.0,
        };
        let tau: f32 = if target > self.alpha { 0.05 } else { 0.3 };
        self.alpha += (target - self.alpha) * (1.0 - (-dt / tau).exp());
        if target == 0.0 && self.alpha < 0.01 {
            self.shown = None;
        }
    }

    /// The key to point at this frame, if any.
    pub fn shown(&self) -> Option<K> {
        self.shown.filter(|_| self.alpha > 0.01)
    }

    fn from(&self, ui: &Ui) -> Option<Pos2> {
        let k = self.shown?;
        self.sliders
            .iter()
            .find(|s| s.0 == k && s.2 == ui.ctx().cumulative_frame_nr())
            .map(|s| pos2(s.1.right() + 3.0, s.1.center().y))
    }

    fn color(&self, a: f32) -> Color32 {
        theme::ACCENT_BRIGHT.gamma_multiply(a * self.alpha)
    }

    /// Point at a vertical line at `x` on the graph.
    pub fn point_x(&self, ui: &Ui, rect: Rect, x: f32) {
        if self.shown().is_none() {
            return;
        }
        let p = ui.painter();
        let x = x.clamp(rect.left(), rect.right());
        p.vline(x, rect.y_range(), Stroke::new(1.0, self.color(0.6)));
        if let Some(from) = self.from(ui) {
            let y = from.y.clamp(rect.top() + 3.0, rect.bottom() - 3.0);
            let pts = [
                from,
                pos2(rect.left() - 4.0, from.y),
                pos2(rect.left(), y),
                pos2(x, y),
            ];
            queue_link(
                ui,
                Shape::dashed_line(&pts, Stroke::new(1.0, self.color(0.4)), 3.0, 3.0),
            );
            p.circle_filled(pos2(x, y), 2.5, self.color(1.0));
        }
    }

    /// Point at a horizontal line at `y` on the graph.
    pub fn point_y(&self, ui: &Ui, rect: Rect, y: f32) {
        if self.shown().is_none() {
            return;
        }
        let p = ui.painter();
        let y = y.clamp(rect.top(), rect.bottom());
        p.hline(rect.x_range(), y, Stroke::new(1.0, self.color(0.6)));
        if let Some(from) = self.from(ui) {
            let pts = [
                from,
                pos2(rect.left() - 4.0, from.y),
                pos2(rect.left() - 4.0, y),
                pos2(rect.left() + 6.0, y),
            ];
            queue_link(
                ui,
                Shape::dashed_line(&pts, Stroke::new(1.0, self.color(0.4)), 3.0, 3.0),
            );
            p.circle_filled(pos2(rect.left() + 6.0, y), 2.5, self.color(1.0));
        }
    }
}

/// Collect cross-column guides, then paint them above the completed workspace.
fn queue_link(ui: &Ui, shapes: Vec<Shape>) {
    ui.data_mut(|d| {
        d.get_temp_mut_or_default::<Vec<Shape>>(egui::Id::unique("parameter_links"))
            .extend(shapes)
    });
}

/// Whether a parameter guide is queued this frame, so other leaders can yield.
pub fn links_pending(ui: &Ui) -> bool {
    ui.data(|d| {
        d.get_temp::<Vec<Shape>>(egui::Id::unique("parameter_links"))
            .is_some_and(|v| !v.is_empty())
    })
}

pub fn paint_links(ui: &Ui, rect: Rect) {
    let shapes = ui.data_mut(|d| {
        std::mem::take(d.get_temp_mut_or_default::<Vec<Shape>>(egui::Id::unique("parameter_links")))
    });
    ui.painter_at(rect).extend(shapes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_links_ignore_hidden_and_previous_frame_sliders() {
        let ctx = egui::Context::default();
        let mut link = Link::<u8>::default();
        let mut value = 0.5;
        for visible in [true, false] {
            let mut output = ctx.run_ui(Default::default(), |ui| {
                if visible {
                    link.slider(ui, 1, egui::Slider::new(&mut value, 0.0..=1.0));
                }
                link.touch(1);
                link.tick(0.1);
                assert_eq!(link.from(ui).is_some(), visible);
            });
            output.textures_delta.clear();
        }
        let mut output = ctx.run_ui(Default::default(), |ui| {
            ui.set_clip_rect(Rect::NOTHING);
            link.slider(ui, 1, egui::Slider::new(&mut value, 0.0..=1.0));
            assert!(link.from(ui).is_none());
        });
        output.textures_delta.clear();
    }

    #[test]
    fn linear_ticks_are_round() {
        let s = Scale::linear((0.0, 600.0), (0.0, 640.0));
        let v: Vec<f32> = s.ticks(64.0).iter().map(|t| t.value).collect();
        assert_eq!(v, [0.0, 100.0, 200.0, 300.0, 400.0, 500.0, 600.0]);
        assert_eq!(s.value(s.px(123.0)).round(), 123.0);
    }

    #[test]
    fn log_ticks_thin_out() {
        let s = Scale::log((20.0, 20_000.0), (0.0, 300.0));
        let t = s.ticks(40.0);
        assert!(t.iter().any(|t| t.value == 100.0 && t.major));
        for w in t.windows(2) {
            assert!(s.px(w[1].value) - s.px(w[0].value) >= 40.0);
        }
    }

    #[test]
    fn labels() {
        assert_eq!(fmt_ms(250.0), "250ms");
        assert_eq!(fmt_ms(1500.0), "1.5s");
        assert_eq!(fmt_hz(1200.0), "1.2k");
        assert_eq!(trim(-0.0), "0");
    }
}

//! Icons drawn as shapes rather than font glyphs: the UI font lacks the
//! characters on Windows, and the symbol ones render as a coloured emoji.

use egui::{Color32, Painter, Pos2, Shape, Stroke, vec2};

/// Which way a [`triangle`] points.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// A filled triangle centred on `c`, half-height `r`, pointing in `dir`.
pub fn triangle(painter: &Painter, c: Pos2, dir: Dir, r: f32, color: Color32) {
    let (a, b, tip) = match dir {
        Dir::Right => (vec2(-r * 0.6, -r), vec2(-r * 0.6, r), vec2(r * 0.8, 0.0)),
        Dir::Left => (vec2(r * 0.6, -r), vec2(r * 0.6, r), vec2(-r * 0.8, 0.0)),
        Dir::Down => (vec2(-r, -r * 0.6), vec2(r, -r * 0.6), vec2(0.0, r * 0.8)),
        Dir::Up => (vec2(-r, r * 0.6), vec2(r, r * 0.6), vec2(0.0, -r * 0.8)),
    };
    let pts = vec![c + a, c + b, c + tip];
    painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Foldout triangle centred on `c`: pointing down when `open`, right otherwise.
pub fn disclosure(painter: &Painter, c: Pos2, open: bool, color: Color32) {
    let dir = if open { Dir::Down } else { Dir::Right };
    triangle(painter, c, dir, 4.0, color);
}

/// Play (right-pointing triangle) or pause (two bars) centred on `c`.
pub fn transport(painter: &Painter, c: Pos2, playing: bool, color: Color32) {
    let r = 5.0;
    if playing {
        triangle(painter, c, Dir::Right, r, color);
    } else {
        for x in [-r * 0.5, r * 0.5] {
            let bar = egui::Rect::from_center_size(c + vec2(x, 0.0), vec2(2.5, r * 2.0));
            painter.rect_filled(bar, 0.0, color);
        }
    }
}

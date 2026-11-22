//! Lifted from KitPool's top bar: gain slider with a unity tick, the L/R
//! peak meter with hold, and the goniometer "phase strip" that fills the
//! rest of the bar while a stereo input is open.

use egui::{Color32, Rect, Shape, Stroke, Ui, pos2, vec2};

use super::theme;
use crate::audio::Input;

/// Gain slider top: +12 dB.
pub const GAIN_MAX: f32 = 4.0;

pub fn to_db(lin: f32) -> f32 {
    if lin <= 0.0 { f32::NEG_INFINITY } else { 20.0 * lin.log10() }
}

pub fn from_db(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Meter ballistics, kept across frames.
#[derive(Default)]
pub struct Meter {
    level: [f32; 2],
    hold: [f32; 2],
}

impl Meter {
    /// The held peak of both channels, linear.
    pub fn hold(&self) -> f32 {
        self.hold[0].max(self.hold[1])
    }

    /// Gain slider and meter.
    pub fn show(&mut self, ui: &mut Ui, input: &Input, gain: &mut f32) {
        gain_slider(ui, gain, |db| {
            format!("Input gain {db} (ahead of every analyser and the trigger). Tick marks 0 dB; double-click to reset to it")
        });
        input.set_gain(*gain);
        self.levels(ui, input.take_peaks(), "Input level after gain");
    }

    /// The phase strip from the cursor to the bar's right edge, full bar
    /// height, while a stereo input is open.
    pub fn phase_strip(&self, ui: &mut Ui, input: &Input) {
        let ys = ui.clip_rect().intersect(ui.max_rect().expand2(vec2(0.0, 8.0))).y_range();
        let strip = Rect::from_x_y_ranges(ui.cursor().left()..=ui.max_rect().right(), ys);
        if strip.width() > 4.0 && input.is_stereo() {
            let mut shapes = Vec::new();
            phase_strip(&input.scope(1024), strip, &mut shapes);
            ui.painter().add(Shape::Vec(shapes));
        }
    }

    /// The L/R peak meter fed with `peaks` since last frame; `what` heads
    /// its tooltip.
    pub fn levels(&mut self, ui: &mut Ui, peaks: [f32; 2], what: &str) {
        // Fast attack, ~24 dB/s fall; the hold mark falls slower.
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let fall = 10f32.powf(-24.0 * dt / 20.0);
        let hold_fall = 10f32.powf(-8.0 * dt / 20.0);
        for k in 0..2 {
            self.level[k] = peaks[k].max(self.level[k] * fall);
            self.hold[k] = peaks[k].max(self.hold[k] * hold_fall);
        }

        let (rect, resp) = ui.allocate_exact_size(vec2(96.0, 12.0), egui::Sense::hover());
        let p = ui.painter();
        const FLOOR_DB: f32 = -48.0;
        const FIRST_TICK_DB: f32 = -24.0;
        let hold_db = to_db(self.hold().max(1e-6));
        // Background and ticks fade out as the held peak falls from the
        // first tick to the floor, so an idle meter is invisible.
        let bg_alpha = ((hold_db - FLOOR_DB) / (FIRST_TICK_DB - FLOOR_DB)).clamp(0.0, 1.0);
        if bg_alpha > 0.0 {
            p.rect_filled(rect, 1.0, ui.visuals().extreme_bg_color.gamma_multiply(bg_alpha));
        }
        let x_of = |lin: f32| {
            let db = to_db(lin.max(1e-6));
            rect.left() + rect.width() * ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
        };
        let bar_h = (rect.height() - 1.0) / 2.0;
        for k in 0..2 {
            let y0 = rect.top() + k as f32 * (bar_h + 1.0);
            let bar = Rect::from_x_y_ranges(rect.x_range(), y0..=y0 + bar_h);
            super::level_bar::paint(
                p,
                bar,
                audio_fraction(self.level[k]),
                (self.hold[k] > 1e-4).then(|| audio_fraction(self.hold[k])),
            );
        }
        // Tick marks at -24, -12 and -6 dB.
        if bg_alpha > 0.0 {
            for db in [FIRST_TICK_DB, -12.0, -6.0] {
                let x = x_of(from_db(db));
                p.line_segment(
                    [pos2(x, rect.top()), pos2(x, rect.bottom())],
                    Stroke::new(1.0, Color32::from_black_alpha(140).gamma_multiply(bg_alpha)),
                );
            }
        }
        resp.on_hover_text(format!(
            "{what}, top L, bottom R (-48..0 dBFS)\nPeak {}",
            if hold_db < -90.0 { "-inf dB".to_owned() } else { format!("{hold_db:.1} dB") }
        ));
    }
}

/// Gain 0..+12 dB. Tick marks at unity; double-click resets to it.
/// `hover` gets the gain as text ("+3.0 dB", "off").
pub fn gain_slider(ui: &mut Ui, gain: &mut f32, hover: impl FnOnce(&str) -> String) -> egui::Response {
    ui.spacing_mut().slider_width = 80.0;
    let r = ui.add(egui::Slider::new(gain, 0.0..=GAIN_MAX).show_value(false).trailing_fill(true));
    // Tick marks at unity gain, above and below the rail. Mirrors egui's
    // handle inset (radius = height / 2.5) so the tick lines up.
    {
        let rect = r.rect;
        let inset = rect.height() / 2.5;
        let x = rect.left() + inset + (rect.width() - 2.0 * inset) / GAIN_MAX;
        let stroke = Stroke::new(1.0, ui.visuals().weak_text_color());
        let p = ui.painter();
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.top() + 3.0)], stroke);
        p.line_segment([pos2(x, rect.bottom() - 3.0), pos2(x, rect.bottom())], stroke);
    }
    let db = to_db(*gain);
    let mut r = r.on_hover_text(hover(&if db.is_finite() { format!("{db:+.1} dB") } else { "off".to_owned() }));
    if r.double_clicked() {
        *gain = 1.0;
        r.mark_changed();
    }
    r
}

/// Goniometer (mid up, side across) scaled so side spans the strip's
/// width; only a thin slice around the centre fits the height. Mono shows
/// as a vertical line, wide stereo spreads out sideways. A thin L/R
/// correlation bar runs along the bottom edge.
fn phase_strip(frames: &[(f32, f32)], strip: Rect, shapes: &mut Vec<Shape>) {
    let c = strip.center();
    let r = strip.width() / 2.0;
    let s = std::f32::consts::FRAC_1_SQRT_2;
    let guide = Stroke::new(1.0, Color32::from_gray(28));
    shapes.push(Shape::line_segment([pos2(c.x, strip.top()), pos2(c.x, strip.bottom())], guide));

    // Cut the trace into runs that stay inside the strip, so lines don't
    // get drawn across the clipped-away parts.
    let stroke = Stroke::new(1.0, theme::WAVE.gamma_multiply(0.45));
    let mut run: Vec<egui::Pos2> = Vec::new();
    for &(l, rr) in frames {
        let pt = c + vec2((rr - l) * s, -(l + rr) * s) * r;
        if strip.contains(pt) {
            run.push(pt);
        } else if !run.is_empty() {
            if run.len() > 1 {
                shapes.push(Shape::line(std::mem::take(&mut run), stroke));
            }
            run.clear();
        }
    }
    if run.len() > 1 {
        shapes.push(Shape::line(run, stroke));
    }

    // Correlation: +1 mono, 0 wide, -1 out of phase.
    let (mut lr, mut ll, mut rr2) = (0.0f32, 0.0f32, 0.0f32);
    for &(l, rr) in frames {
        lr += l * rr;
        ll += l * l;
        rr2 += rr * rr;
    }
    if ll * rr2 > 1e-12 {
        let corr = (lr / (ll * rr2).sqrt()).clamp(-1.0, 1.0);
        let x = c.x + corr * strip.width() / 2.0;
        let col = if corr < 0.0 { Color32::from_rgb(222, 60, 50) } else { theme::ACCENT_BRIGHT };
        let bar = Rect::from_x_y_ranges(c.x.min(x)..=c.x.max(x), strip.bottom() - 2.0..=strip.bottom());
        shapes.push(Shape::rect_filled(bar, 0.0, col.gamma_multiply(0.6)));
    }
}

/// Per-channel levels for the channel picker, with the same ballistics as
/// the main meter.
#[derive(Default)]
pub struct ChannelMeters {
    level: Vec<f32>,
    hold: Vec<f32>,
}

impl ChannelMeters {
    pub fn update(&mut self, peaks: &[f32], dt: f32) {
        self.level.resize(peaks.len(), 0.0);
        self.hold.resize(peaks.len(), 0.0);
        let fall = 10f32.powf(-24.0 * dt / 20.0);
        let hold_fall = 10f32.powf(-8.0 * dt / 20.0);
        for (k, &p) in peaks.iter().enumerate() {
            self.level[k] = p.max(self.level[k] * fall);
            self.hold[k] = p.max(self.hold[k] * hold_fall);
        }
    }

    /// A thin horizontal bar for channel `k` (-48..0 dBFS).
    pub fn bar(&self, ui: &mut Ui, k: usize, size: egui::Vec2) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
        let (level, hold) = (self.level.get(k).copied().unwrap_or(0.0), self.hold.get(k).copied().unwrap_or(0.0));
        let p = ui.painter();
        p.rect_filled(rect, 1.0, ui.visuals().extreme_bg_color);
        super::level_bar::paint(
            p,
            rect,
            audio_fraction(level),
            (hold > 1e-4).then(|| audio_fraction(hold)),
        );
        let db = to_db(hold.max(1e-6));
        resp.on_hover_text(if db < -90.0 { "-inf dB".to_owned() } else { format!("Peak {db:.1} dB") })
    }
}

/// Map the audio display range (-48..0 dBFS) onto a normalized bar.
fn audio_fraction(level: f32) -> f32 {
    ((to_db(level.max(1e-6)) + 48.0) / 48.0).clamp(0.0, 1.0)
}

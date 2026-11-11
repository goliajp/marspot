//! The controls every panel shares: buttons, segmented control,
//! toggle, stepper, danger icon button, status chip.
//!
//! Each control is two functions.  A **geometry** function says how
//! big it is and where its parts are, with no painter — a panel calls
//! it from its layout walk and again from its hit-test, so a control is
//! clickable exactly where it is drawn.  A **paint** function takes
//! the rect that geometry produced and the control's state.
//!
//! Sizes come from `ui::theme::spec::control`, colours from
//! `panel_palette`.  Selected and on are the accent everywhere; the
//! status colours are for status.
//!
//! All rects and widths are physical pixels.

use marspot_term::layout::Rect;

use crate::font_cache::FontCache;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;
use crate::ui::strings::{t, Msg};
use crate::ui::system::macos::icons::{self, Symbol};
use crate::ui::theme::spec::{self, control as c};
use crate::ui::theme::PanelText;

/// Measures a string in the UI font: `(text, pt, weight) -> width` in
/// physical pixels.
///
/// Threaded into the geometry functions so that layout and hit-test
/// measure the same way the painter does; a width guessed from a
/// character count is how a label overruns the button drawn for it.
pub type Measure<'a> = &'a mut dyn FnMut(&str, f64, u16) -> f64;

/// The measurer over a font cache — what both the renderer and the
/// click handler hand to the geometry functions.
pub fn font_measure(font: &mut FontCache) -> impl FnMut(&str, f64, u16) -> f64 + '_ {
    move |s, pt, weight| {
        font.measure_ui_text_at_size(s, weight, crate::font_shape::ShapeOptions::default(), pt)
    }
}

fn role_width(m: Measure<'_>, role: PanelText, s: &str) -> f64 {
    m(s, role.pt(), role.weight())
}

/// `s` centred in `rect`, on its cap height.
fn centred_text(p: &mut ViewPainter, rect: Rect, s: &str, pt: f64, weight: u16, color: [f32; 4]) {
    let w = p.ui_text_width_at(s, pt, weight) as f64;
    let baseline = p.ui_baseline_centred_for(s, rect.y_top as f32, rect.h as f32, pt);
    p.ui_text_at((rect.x + (rect.w - w) * 0.5) as f32, baseline, s, pt, weight, color);
}

fn fill(p: &mut ViewPainter, rect: Rect, color: [f32; 4], radius_pt: f64) {
    p.fill_rounded_rect(rect, color, spec::px(radius_pt) as f32, (palette::none(), 0.0));
}

/// A × `mark_pt` across, centred in `rect`: the system's close symbol,
/// at the point size whose ink is that wide.
pub fn paint_cross(p: &mut ViewPainter, rect: Rect, mark_pt: f64, color: [f32; 4]) {
    icons::paint(p, Symbol::Close, rect, spec::px(mark_pt) / icons::MARK_INK, color);
}

// ─── Buttons ────────────────────────────────────────────────────

/// Which of the two text buttons.  A panel has at most one primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
}

/// `(width, height)` of a button holding `label`: as wide as the label
/// plus its padding, never under the minimum.
pub fn button_size(label: &str, m: Measure<'_>) -> (f64, f64) {
    let w = role_width(m, c::BUTTON_TEXT, label) + 2.0 * spec::px(c::BUTTON_PAD_X);
    (w.max(spec::px(c::BUTTON_MIN_W)), spec::px(c::BUTTON_H))
}

pub fn paint_button(
    p: &mut ViewPainter,
    rect: Rect,
    label: &str,
    kind: ButtonKind,
    hovered: bool,
    disabled: bool,
) {
    let base = match kind {
        ButtonKind::Primary if !disabled => palette::accent(),
        _ => palette::segment_bg(),
    };
    let bg = if hovered && !disabled { palette::hover(base) } else { base };
    let fg = match kind {
        _ if disabled => palette::disabled_fg(),
        ButtonKind::Primary => palette::on_accent(),
        _ => palette::fg(),
    };
    fill(p, rect, bg, c::BUTTON_RADIUS);
    centred_text(p, rect, label, c::BUTTON_TEXT.pt(), c::BUTTON_TEXT.weight(), fg);
}

// ─── Segmented control ──────────────────────────────────────────

/// Width of the segment holding `label` — each is sized to its own
/// label, so a long option is not squeezed and a short one not padded
/// out.
pub fn segment_width(label: Msg, m: Measure<'_>) -> f64 {
    role_width(m, c::SEGMENT_TEXT, t(label)) + 2.0 * spec::px(c::SEGMENT_PAD_X)
}

/// `(width, height)` of a whole segmented control.
pub fn segmented_size(labels: &[Msg], m: Measure<'_>) -> (f64, f64) {
    let gaps = spec::px(c::SEGMENT_GAP) * labels.len().saturating_sub(1) as f64;
    let w = labels.iter().map(|l| segment_width(*l, m)).sum::<f64>() + gaps;
    (w, spec::px(c::SEGMENT_H))
}

/// The `n`th segment of the control at `control`.
pub fn segment_rect(control: Rect, n: usize, labels: &[Msg], m: Measure<'_>) -> Rect {
    let gap = spec::px(c::SEGMENT_GAP);
    let x = labels.iter().take(n).map(|l| segment_width(*l, m) + gap).sum::<f64>();
    Rect {
        x: control.x + x,
        y_top: control.y_top,
        w: labels.get(n).map(|l| segment_width(*l, m)).unwrap_or(0.0),
        h: control.h,
    }
}

/// Which segment `(x, y)` is on.
pub fn segment_at(control: Rect, labels: &[Msg], m: Measure<'_>, x: f64, y: f64) -> Option<usize> {
    (0..labels.len()).find(|&n| segment_rect(control, n, labels, m).contains(x, y))
}

/// `selected` is `None` when the value in force is not one of the
/// options.  A disabled control shows no selection: what it would pick
/// is not in effect.
pub fn paint_segmented(
    p: &mut ViewPainter,
    control: Rect,
    labels: &[Msg],
    selected: Option<usize>,
    hovered: Option<usize>,
    disabled: bool,
) {
    let (pt, weight) = (c::SEGMENT_TEXT.pt(), c::SEGMENT_TEXT.weight());
    let pad = 2.0 * spec::px(c::SEGMENT_PAD_X);
    let mut x = control.x;
    for (n, label) in labels.iter().enumerate() {
        let label = t(*label);
        let w = p.ui_text_width_at(label, pt, weight) as f64 + pad;
        let seg = Rect { x, y_top: control.y_top, w, h: control.h };
        x += w + spec::px(c::SEGMENT_GAP);
        let picked = !disabled && selected == Some(n);
        let base = if picked { palette::accent() } else { palette::segment_bg() };
        let bg = if !disabled && hovered == Some(n) { palette::hover(base) } else { base };
        let fg = if disabled {
            palette::disabled_fg()
        } else if picked {
            palette::on_accent()
        } else {
            palette::fg_sec()
        };
        fill(p, seg, bg, c::SEGMENT_RADIUS);
        centred_text(p, seg, label, pt, weight, fg);
    }
}

// ─── Toggle ─────────────────────────────────────────────────────

/// `(width, height)` of a toggle.
pub fn toggle_size() -> (f64, f64) {
    (spec::px(c::TOGGLE_W), spec::px(c::TOGGLE_H))
}

/// The knob's rect inside the toggle at `rect`.
pub fn toggle_knob_rect(rect: Rect, on: bool) -> Rect {
    let d = spec::px(c::TOGGLE_KNOB);
    let pad = (rect.h - d) * 0.5;
    Rect {
        x: if on { rect.x + rect.w - d - pad } else { rect.x + pad },
        y_top: rect.y_top + pad,
        w: d,
        h: d,
    }
}

pub fn paint_toggle(p: &mut ViewPainter, rect: Rect, on: bool, disabled: bool) {
    let track = if on { palette::accent() } else { palette::toggle_off() };
    let knob = palette::knob();
    let (track, knob) = if disabled {
        (palette::faded(track), palette::faded(knob))
    } else {
        (track, knob)
    };
    p.fill_rounded_rect(rect, track, (rect.h * 0.5) as f32, (palette::none(), 0.0));
    let k = toggle_knob_rect(rect, on);
    p.fill_rounded_rect(k, knob, (k.h * 0.5) as f32, (palette::none(), 0.0));
}

// ─── Stepper ────────────────────────────────────────────────────

/// One of a stepper's two buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepperPart {
    Minus,
    Plus,
}

/// Where a stepper's parts are.
#[derive(Debug, Clone, Copy)]
pub struct StepperRects {
    pub minus: Rect,
    pub value: Rect,
    pub plus: Rect,
}

impl StepperRects {
    /// Which button `(x, y)` is on.
    pub fn hit(&self, x: f64, y: f64) -> Option<StepperPart> {
        if self.minus.contains(x, y) {
            Some(StepperPart::Minus)
        } else if self.plus.contains(x, y) {
            Some(StepperPart::Plus)
        } else {
            None
        }
    }
}

/// `(width, height)` of a stepper.
pub fn stepper_size() -> (f64, f64) {
    (spec::px(2.0 * c::STEPPER_BUTTON + c::STEPPER_VALUE_W), spec::px(c::STEPPER_BUTTON))
}

/// The parts of the stepper whose top-left corner is `(x, y_top)`.
pub fn stepper_rects(x: f64, y_top: f64) -> StepperRects {
    let b = spec::px(c::STEPPER_BUTTON);
    let v = spec::px(c::STEPPER_VALUE_W);
    let part = |x: f64, w: f64| Rect { x, y_top, w, h: b };
    StepperRects {
        minus: part(x, b),
        value: part(x + b, v),
        plus: part(x + b + v, b),
    }
}

/// Paints the stepper at `(x, y_top)` and returns its parts.  A button
/// that cannot go further (`*_enabled == false`) is drawn disabled.
pub fn paint_stepper(
    p: &mut ViewPainter,
    x: f64,
    y_top: f64,
    value: &str,
    minus_enabled: bool,
    plus_enabled: bool,
    hovered: Option<StepperPart>,
) -> StepperRects {
    let r = stepper_rects(x, y_top);
    let mut button = |rect: Rect, glyph: &str, part: StepperPart, enabled: bool| {
        let base = palette::segment_bg();
        let bg = if enabled && hovered == Some(part) { palette::hover(base) } else { base };
        let fg = if enabled { palette::fg() } else { palette::disabled_fg() };
        fill(p, rect, bg, c::BUTTON_RADIUS);
        centred_text(p, rect, glyph, c::BUTTON_TEXT.pt(), c::BUTTON_TEXT.weight(), fg);
    };
    button(r.minus, "−", StepperPart::Minus, minus_enabled);
    button(r.plus, "+", StepperPart::Plus, plus_enabled);
    // The value is a number that changes under the pointer: in the
    // terminal's fixed-width font it does not shift as digits change.
    p.text_in(r.value, value, palette::fg(), marspot_term::layout::Alignment::Center);
    r
}

// ─── Danger icon button ─────────────────────────────────────────

/// `(width, height)` of the danger icon button.
pub fn danger_button_size() -> (f64, f64) {
    (spec::px(c::DANGER_SIZE), spec::px(c::DANGER_SIZE))
}

/// `(width, height)` of the danger icon button at the end of a table
/// row `row_h` tall: its own size where the row has room for it,
/// otherwise the largest square that leaves clear space above and
/// below.  A table row is measured in terminal cells and at the usual
/// font is shorter than the button.
pub fn danger_button_size_in_row(row_h: f64) -> (f64, f64) {
    let fits = (row_h - 2.0 * spec::px(spec::space::S2)).floor().max(0.0);
    let side = spec::px(c::DANGER_SIZE).min(fits);
    (side, side)
}

pub fn paint_danger_button(p: &mut ViewPainter, rect: Rect, hovered: bool) {
    let (bg, fg) = if hovered {
        (palette::danger(), palette::on_accent())
    } else {
        (palette::danger_rest_bg(), palette::severity_text(2))
    };
    // A button cut down to fit a row keeps its proportions.
    let k = rect.w / spec::px(c::DANGER_SIZE);
    fill(p, rect, bg, c::DANGER_RADIUS * k);
    paint_cross(p, rect, c::DANGER_MARK * k, fg);
}

/// The danger button's × alone, in `color`: how the button is drawn
/// where a column of them at rest would be a column of red, until the
/// pointer is on one.
pub fn paint_danger_mark(p: &mut ViewPainter, rect: Rect, color: [f32; 4]) {
    let k = rect.w / spec::px(c::DANGER_SIZE);
    paint_cross(p, rect, c::DANGER_MARK * k, color);
}

// ─── Status chip ────────────────────────────────────────────────

/// `(width, height)` of a status chip holding `label`.
pub fn chip_size(label: &str, m: Measure<'_>) -> (f64, f64) {
    let w = m(label, c::CHIP_TEXT.pt(), c::CHIP_WEIGHT) + 2.0 * spec::px(c::CHIP_PAD_X);
    (w, spec::px(c::CHIP_H))
}

/// `severity`: 0 fine, 1 approaching a limit, anything else a problem
/// — the same scale as `panel_palette::severity_color`.
pub fn paint_chip(p: &mut ViewPainter, rect: Rect, label: &str, severity: u8) {
    fill(p, rect, palette::chip_bg(severity), c::CHIP_RADIUS);
    centred_text(
        p, rect, label, c::CHIP_TEXT.pt(), c::CHIP_WEIGHT, palette::severity_text(severity),
    );
}

// ─── Key cap ────────────────────────────────────────────────────

/// `(width, height)` of a key cap holding `label` — a chord such as
/// `⇧⌘L`.  Set in the terminal's font, where every glyph is a whole
/// number of cells wide, so the cap is sized from the cell.
pub fn keycap_size(label: &str, cell_w: f64, cell_h: f64) -> (f64, f64) {
    let cells: usize = label.chars().map(|ch| crate::grid::char_width(ch).max(1) as usize).sum();
    (cells as f64 * cell_w + 2.0 * spec::px(c::KEYCAP_PAD_X), cell_h + 2.0 * spec::px(c::KEYCAP_PAD_Y))
}

/// Paint a key cap whose right edge is `right`, `h` tall from `top`,
/// and return its left edge, so a row of caps is set right to left.
///
/// The glyphs are in the interface font, measured here.  In the
/// terminal's font the modifier signs are squeezed into one cell each
/// and came out too small to tell apart; the cap's slot is still sized
/// from the cell (`keycap_size`), which is what fixes the row's height.
/// `filled` false draws the glyphs alone: what joins two caps.
pub fn paint_keycap_ending_at(p: &mut ViewPainter, right: f64, top: f64, h: f64, label: &str, filled: bool) -> f64 {
    let pt = c::KEYCAP_TEXT.pt();
    let text_w = p.ui_text_width_at(label, pt, c::KEYCAP_WEIGHT) as f64;
    // never narrower than tall: a one-glyph cap is a square key
    let w = (text_w + 2.0 * spec::px(c::KEYCAP_PAD_X)).max(h).round();
    let rect = Rect { x: right - w, y_top: top, w, h };
    if filled {
        fill(p, rect, palette::keycap_bg(), c::KEYCAP_RADIUS);
    }
    centred_text(p, rect, label, pt, c::KEYCAP_WEIGHT, palette::fg_sec());
    rect.x
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Six units per character at any size: enough to make widths
    /// depend on the label.
    fn fake(s: &str, _pt: f64, _w: u16) -> f64 {
        s.chars().count() as f64 * 6.0
    }

    #[test]
    fn controls_button_is_never_narrower_than_the_minimum() {
        let (w, h) = button_size("OK", &mut fake);
        assert_eq!(w, spec::px(c::BUTTON_MIN_W));
        assert_eq!(h, spec::px(c::BUTTON_H));
        let long = "Apply to every window in this session";
        let (w, _) = button_size(long, &mut fake);
        assert_eq!(w, fake(long, 0.0, 0) + 2.0 * spec::px(c::BUTTON_PAD_X));
    }

    #[test]
    fn controls_segments_tile_the_control_with_the_gap_between() {
        let labels = [Msg::Idle15m, Msg::Idle30m, Msg::Idle1h, Msg::IdleNever];
        let (w, h) = segmented_size(&labels, &mut fake);
        let control = Rect { x: 100.0, y_top: 40.0, w, h };
        let gap = spec::px(c::SEGMENT_GAP);
        let mut x = control.x;
        for n in 0..labels.len() {
            let r = segment_rect(control, n, &labels, &mut fake);
            assert_eq!(r.x, x, "segment {n} starts where the previous one and a gap end");
            assert_eq!((r.y_top, r.h), (control.y_top, control.h));
            x = r.x + r.w + gap;
        }
        assert!((x - gap - (control.x + control.w)).abs() < 1e-9, "the last one ends the control");
    }

    #[test]
    fn controls_segment_hit_is_the_segment_drawn_there() {
        let labels = [Msg::DimOff, Msg::DimLight, Msg::DimNormal, Msg::DimDeep];
        let (w, h) = segmented_size(&labels, &mut fake);
        let control = Rect { x: 10.0, y_top: 10.0, w, h };
        for n in 0..labels.len() {
            let r = segment_rect(control, n, &labels, &mut fake);
            let hit = segment_at(control, &labels, &mut fake, r.x + r.w * 0.5, r.y_top + r.h * 0.5);
            assert_eq!(hit, Some(n));
        }
        // The gap between two segments belongs to neither.
        let first = segment_rect(control, 0, &labels, &mut fake);
        let in_gap = first.x + first.w + spec::px(c::SEGMENT_GAP) * 0.5;
        assert_eq!(segment_at(control, &labels, &mut fake, in_gap, 12.0), None);
        assert_eq!(segment_at(control, &labels, &mut fake, 0.0, 0.0), None);
    }

    #[test]
    fn controls_toggle_knob_stays_inside_the_track_at_either_end() {
        let (w, h) = toggle_size();
        let track = Rect { x: 50.0, y_top: 20.0, w, h };
        let off = toggle_knob_rect(track, false);
        let on = toggle_knob_rect(track, true);
        for k in [off, on] {
            assert!(k.x >= track.x && k.x + k.w <= track.x + track.w);
            assert!(k.y_top >= track.y_top && k.y_top + k.h <= track.y_top + track.h);
            assert_eq!(k.w, spec::px(c::TOGGLE_KNOB));
        }
        assert!(on.x > off.x, "on is the right end");
        // Same margin at the end it rests against as above and below.
        assert!(((off.x - track.x) - (off.y_top - track.y_top)).abs() < 1e-9);
        assert!(((track.x + track.w - on.x - on.w) - (off.x - track.x)).abs() < 1e-9);
    }

    #[test]
    fn controls_stepper_parts_abut_and_hit_only_the_buttons() {
        let (w, h) = stepper_size();
        let r = stepper_rects(200.0, 30.0);
        assert_eq!(r.minus.x, 200.0);
        assert_eq!(r.value.x, r.minus.x + r.minus.w);
        assert_eq!(r.plus.x, r.value.x + r.value.w);
        assert_eq!(r.plus.x + r.plus.w, 200.0 + w);
        assert_eq!((r.minus.h, r.value.h, r.plus.h), (h, h, h));
        assert_eq!(r.minus.w, r.minus.h, "the buttons are square");
        let mid = 30.0 + h * 0.5;
        assert_eq!(r.hit(r.minus.x + 1.0, mid), Some(StepperPart::Minus));
        assert_eq!(r.hit(r.plus.x + 1.0, mid), Some(StepperPart::Plus));
        assert_eq!(r.hit(r.value.x + 1.0, mid), None, "the value is not a button");
        assert_eq!(r.hit(r.minus.x + 1.0, 30.0 + h + 1.0), None);
    }

    #[test]
    fn controls_keycap_is_its_glyphs_plus_its_padding() {
        let (w, h) = keycap_size("⇧⌘L", 10.0, 20.0);
        assert_eq!(w, 30.0 + 2.0 * spec::px(c::KEYCAP_PAD_X));
        assert_eq!(h, 20.0 + 2.0 * spec::px(c::KEYCAP_PAD_Y));
        assert_eq!(keycap_size("Esc", 10.0, 20.0).0 - keycap_size("⌘W", 10.0, 20.0).0, 10.0);
    }

    #[test]
    fn controls_chip_and_danger_button_take_their_spec_sizes() {
        let (w, h) = chip_size("LIMIT", &mut fake);
        assert_eq!(w, 30.0 + 2.0 * spec::px(c::CHIP_PAD_X));
        assert_eq!(h, spec::px(c::CHIP_H));
        assert_eq!(danger_button_size(), (spec::px(c::DANGER_SIZE), spec::px(c::DANGER_SIZE)));
    }

    #[test]
    fn controls_danger_button_fits_the_row_it_ends() {
        let full = spec::px(c::DANGER_SIZE);
        let clear = 2.0 * spec::px(spec::space::S2);
        // A row with room to spare gets the button at its own size.
        assert_eq!(danger_button_size_in_row(full + clear + 10.0), (full, full));
        // A shorter one gets a square that leaves the clear space.
        let (w, h) = danger_button_size_in_row(21.6);
        assert_eq!(w, h);
        assert_eq!(w, (21.6 - clear).floor());
        assert!(w < full);
        assert_eq!(danger_button_size_in_row(1.0), (0.0, 0.0));
    }

    /// How many colours a source file writes out by hand: an opening
    /// bracket straight into `0.` or `1.` is the start of an RGBA
    /// array.
    fn colour_literals(src: &str) -> usize {
        ['0', '1'].iter().map(|d| src.matches(&format!("[{d}.")).count()).sum()
    }

    #[test]
    fn controls_source_has_no_colour_literals() {
        // Assembled, so that this file does not contain one itself.
        let literal = format!("let c = [{}.13, 0.14, 0.17, 1.0];", 0);
        assert_eq!(colour_literals(&literal), 1, "the search finds a literal when there is one");
        assert_eq!(colour_literals(include_str!("controls.rs")), 0);
    }
}

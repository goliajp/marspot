//! The layout panel: pick the `(columns, rows)` shape of the pane grid
//! and which pane sits in which slot.
//!
//! Geometry and hit-testing here, painting in `layout_modal_paint`.
//! The frame and header are the shared panel chrome's; under them are
//! two groups — **Grid**, a card with a stepper per axis, and
//! **Arrangement**, one small card per slot, dragged to swap — and a
//! footer with the pane count and the one primary button.
//!
//! The panel holds no state.  [`LayoutModal::layout`] returns every
//! rect from the pending shape; the painter draws those rects and the
//! click handler asks [`LayoutModal::hover_at`] about the same ones,
//! so a control reacts exactly where it is drawn.
//!
//! All rects are physical pixels.

use marspot_term::layout::Rect;

use crate::ui::components::controls::{self, Measure, StepperPart, StepperRects};
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::components::surface_stack;
use crate::ui::strings::{self, t, Arg, Msg};
use crate::ui::theme::spec::{self, card, content, space};

/// How far the steppers go.
///
/// The data path has no cap of its own — `Layout::build` takes any
/// shape — so this is only the panel's range.  `SESSION_COUNT_HARD_CAP`
/// is kept at `GRID_MAX²` so the panel can never offer a shape the app
/// would refuse to fill.  A grid is slots, not sessions: a 9×9 window
/// with three panes has three processes and 78 empty seats.
pub const GRID_MIN: usize = 1;
pub const GRID_MAX: usize = 9;

// titled by its command: the panel and the menu item are one name
pub const TITLE: Msg = Msg::CmdLayout;
pub const GRID_HEADING: Msg = Msg::LayoutGrid;
pub const ARRANGEMENT_HEADING: Msg = Msg::LayoutArrangement;
pub const APPLY: Msg = Msg::LayoutApply;

/// What is this panel's own, in points.  Everything else — header,
/// paddings, card, control sizes — is the shared spec's.
pub mod metric {
    use super::GRID_MAX;
    use crate::ui::theme::spec::{card, content, control, space};
    use crate::ui::theme::PanelText;

    /// The widths a panel comes in, narrowest first.  The panel takes
    /// the first one its pane cards fit.
    pub const WIDTHS: [f64; 3] = [360.0, 480.0, 720.0];
    pub const WIDEST: f64 = WIDTHS[WIDTHS.len() - 1];

    pub const ROW_LABEL: PanelText = PanelText::Label;
    pub const CARD_LABEL: PanelText = PanelText::Item;
    pub const CAPTION: PanelText = PanelText::Caption;

    /// A row of the Grid card: the stepper and the row's padding
    /// above and below it.  A pane card is the same height, so the two
    /// groups read as rows of one size.
    pub const ROW_H: f64 = 2.0 * card::ROW_PAD_Y + control::STEPPER_BUTTON;
    /// Between two pane cards, across and down.
    pub const CARD_GAP: f64 = space::CARD_TO_CARD;
    /// Narrowest a pane card is asked to be: what each of `GRID_MAX`
    /// columns gets in the widest panel.  Derived rather than chosen,
    /// so the widest grid is exactly the one that still honours it.
    pub const CARD_MIN_W: f64 = (WIDEST - 2.0 * content::PAD_X - (GRID_MAX - 1) as f64 * CARD_GAP)
        / GRID_MAX as f64;
}

/// One of the grid's two dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Columns,
    Rows,
}

impl Axis {
    pub fn label(self) -> Msg {
        match self {
            Axis::Columns => Msg::LayoutColumns,
            Axis::Rows => Msg::LayoutRows,
        }
    }
}

/// What the pointer is on: nothing the panel reacts to, the header's
/// close button, a stepper button, Apply, or the pane card of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hover {
    #[default]
    None,
    Close,
    Step(Axis, StepperPart),
    Apply,
    Card(usize),
}

/// One row of the Grid card.
#[derive(Debug, Clone, Copy)]
pub struct StepperRow {
    pub axis: Axis,
    /// The row's full band inside the card.
    pub band: Rect,
    /// Right-aligned in the card, centred in the band.
    pub stepper: StepperRects,
}

pub struct LayoutModal {
    pub chrome: PanelChrome,
    /// Top of the "Grid" heading's line.
    pub grid_heading_top: f64,
    pub grid_card: Rect,
    /// Columns, then Rows.
    pub rows: [StepperRow; 2],
    /// Top of the "Arrangement" heading's line.
    pub arrangement_heading_top: f64,
    /// Bounding rect of the pane cards.
    pub card_grid: Rect,
    /// One rect per slot, row-major; `cols * rows` of them.
    pub cards: Vec<Rect>,
    /// The footer row: the caption at its left, Apply at its right.
    pub footer: Rect,
    pub apply_btn: Rect,
}

impl LayoutModal {
    /// The panel for a pending `cols × rows` grid, centred in the
    /// window.  `label_w` is the width of the widest card label (see
    /// [`widest_label`]); `top_inset` the height of the window's own
    /// chrome.
    ///
    /// A pane card is a project name, so the name decides how wide a
    /// card wants to be and the panel takes the narrowest of its fixed
    /// widths that gives every card that much.  When even the widest
    /// does not, the cards share what there is and the labels are cut
    /// ([`fit_label`]): the cards never leave the content area.
    pub fn layout(
        window_w: f64,
        window_h: f64,
        top_inset: f64,
        cols: usize,
        rows: usize,
        label_w: f64,
        m: Measure<'_>,
    ) -> Self {
        Self::layout_moved(window_w, window_h, top_inset, cols, rows, label_w, m, (0.0, 0.0))
    }

    /// [`Self::layout`] for a panel whose header has been dragged
    /// `offset` from the middle.
    #[allow(clippy::too_many_arguments)]
    pub fn layout_moved(
        window_w: f64,
        window_h: f64,
        top_inset: f64,
        cols: usize,
        rows: usize,
        label_w: f64,
        m: Measure<'_>,
        offset: (f64, f64),
    ) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let (gaps_x, gaps_y) = ((cols - 1) as f64, (rows - 1) as f64);
        let gap = spec::px(metric::CARD_GAP);
        let row_h = spec::px(metric::ROW_H);
        let heading_h = panel_chrome::heading_height();
        let (apply_w, apply_h) = controls::button_size(t(APPLY), m);

        let pad_x = 2.0 * spec::px(content::PAD_X);
        let want_card_w =
            spec::px(metric::CARD_MIN_W).max(label_w + 2.0 * spec::px(card::ROW_PAD_X));
        let want_w = cols as f64 * want_card_w + gaps_x * gap + pad_x;
        let panel_w = metric::WIDTHS
            .iter()
            .map(|w| spec::px(*w))
            .find(|w| *w >= want_w)
            .unwrap_or(spec::px(metric::WIDEST));

        // Everything but the pane cards, top to bottom: heading, the
        // two-row card, heading, then the footer under the cards.
        let fixed_h = 2.0 * heading_h
            + 2.0 * row_h
            + spec::px(space::CARD_TO_HEADING)
            + spec::px(space::CARD_TO_FOOTER)
            + apply_h;
        let block_h = rows as f64 * row_h + gaps_y * gap;
        let frame = panel_chrome::frame_rect(
            window_w,
            window_h,
            panel_w,
            panel_chrome::frame_height(fixed_h + block_h),
            top_inset,
        );
        let frame = surface_stack::place(frame, offset, window_w, window_h, top_inset);
        let chrome = PanelChrome::new(frame, top_inset);
        let c = chrome.content;

        // A window too short for the panel got a shorter frame.  The
        // two groups' controls and the footer keep their size; the
        // pane cards and the gaps between them shrink together into
        // what is left, so the last row never runs into the footer.
        let fit = ((c.h - fixed_h) / block_h).clamp(0.0, 1.0);
        let (card_h, gap_y) = (row_h * fit, gap * fit);
        let card_w = ((c.w - gaps_x * gap) / cols as f64).max(0.0);

        let grid_heading_top = c.y_top;
        let grid_card = Rect { y_top: grid_heading_top + heading_h, h: 2.0 * row_h, ..c };
        let (stepper_w, stepper_h) = controls::stepper_size();
        let stepper_row = |axis: Axis, n: f64| {
            let band = Rect { y_top: grid_card.y_top + n * row_h, h: row_h, ..grid_card };
            StepperRow {
                axis,
                band,
                stepper: controls::stepper_rects(
                    band.x + band.w - spec::px(card::ROW_PAD_X) - stepper_w,
                    band.y_top + (band.h - stepper_h) * 0.5,
                ),
            }
        };
        let arrangement_heading_top =
            grid_card.y_top + grid_card.h + spec::px(space::CARD_TO_HEADING);
        let card_grid = Rect {
            y_top: arrangement_heading_top + heading_h,
            h: rows as f64 * card_h + gaps_y * gap_y,
            ..c
        };
        let cards = (0..rows * cols)
            .map(|slot| Rect {
                x: card_grid.x + (slot % cols) as f64 * (card_w + gap),
                y_top: card_grid.y_top + (slot / cols) as f64 * (card_h + gap_y),
                w: card_w,
                h: card_h,
            })
            .collect();
        // Pinned to the content's bottom edge; with room for the whole
        // panel that is one footer gap under the last row of cards.
        let footer = Rect { y_top: c.y_top + c.h - apply_h, h: apply_h, ..c };
        Self {
            chrome,
            grid_heading_top,
            grid_card,
            rows: [stepper_row(Axis::Columns, 0.0), stepper_row(Axis::Rows, 1.0)],
            arrangement_heading_top,
            card_grid,
            cards,
            footer,
            apply_btn: Rect { x: footer.x + footer.w - apply_w, w: apply_w, ..footer },
        }
    }

    /// What `(x, y)` is on.  The painter highlights it and a click
    /// acts on it, so the two cannot disagree.
    pub fn hover_at(&self, x: f64, y: f64) -> Hover {
        if self.chrome.hit_close(x, y) {
            return Hover::Close;
        }
        if self.apply_btn.contains(x, y) {
            return Hover::Apply;
        }
        for row in &self.rows {
            if let Some(part) = row.stepper.hit(x, y) {
                return Hover::Step(row.axis, part);
            }
        }
        self.cards.iter().position(|r| r.contains(x, y)).map_or(Hover::None, Hover::Card)
    }

    /// Is `(x, y)` on the panel at all?  A click that is not closes it.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.chrome.frame.contains(x, y)
    }

    /// The slot whose centre is closest to `(x, y)`.  `None` only when
    /// there are no cards.
    pub fn nearest_card(&self, x: f64, y: f64) -> Option<usize> {
        let d2 = |r: &Rect| (r.x + r.w * 0.5 - x).powi(2) + (r.y_top + r.h * 0.5 - y).powi(2);
        (0..self.cards.len()).min_by(|a, b| d2(&self.cards[*a]).total_cmp(&d2(&self.cards[*b])))
    }

    /// The card lifted out of `from_slot`, where the pointer at
    /// `mouse` holds it `grab` from its top-left corner.
    pub fn lifted_card(&self, from_slot: usize, mouse: (f64, f64), grab: (f64, f64)) -> Option<Rect> {
        let card = self.cards.get(from_slot)?;
        Some(Rect { x: mouse.0 - grab.0, y_top: mouse.1 - grab.1, ..*card })
    }

    /// Where that card would land: the slot nearest its centre.
    /// `None` while it is still over its own slot.
    pub fn drop_target(&self, from_slot: usize, mouse: (f64, f64), grab: (f64, f64)) -> Option<usize> {
        let held = self.lifted_card(from_slot, mouse, grab)?;
        self.nearest_card(held.x + held.w * 0.5, held.y_top + held.h * 0.5)
            .filter(|to| *to != from_slot)
    }
}

/// `value` after a click on `part` of its stepper, or `None` when it
/// is already at that end of the range.
pub fn step(value: usize, part: StepperPart) -> Option<usize> {
    match part {
        StepperPart::Minus if value > GRID_MIN => Some(value - 1),
        StepperPart::Plus if value < GRID_MAX => Some(value + 1),
        _ => None,
    }
}

/// The footer's caption: `4 × 1 = 4 panes`.
pub fn caption(cols: usize, rows: usize) -> String {
    strings::text(Arg::PaneCount { cols, rows })
}

fn label_width(s: &str, m: Measure<'_>) -> f64 {
    m(s, metric::CARD_LABEL.pt(), metric::CARD_LABEL.weight())
}

/// Width of the widest card label, physical px.
pub fn widest_label(titles: &[String], m: Measure<'_>) -> f64 {
    titles.iter().map(|t| label_width(t, m)).fold(0.0, f64::max)
}

/// The room a pane card `card_w` wide has for its label.
pub fn label_room(card_w: f64) -> f64 {
    (card_w - 2.0 * spec::px(card::ROW_PAD_X)).max(0.0)
}

/// `s` as it is drawn in `max_w` px: whole when it fits, otherwise its
/// longest beginning that fits with an ellipsis after it.  Room for
/// nothing still gets the ellipsis — a blank card would say the slot
/// is empty.
pub fn fit_label(s: &str, max_w: f64, m: Measure<'_>) -> String {
    if s.is_empty() || label_width(s, m) <= max_w {
        return s.to_string();
    }
    let ends: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    let cut = |n: usize| format!("{}…", &s[..ends[n]]);
    // Widths grow with the number of characters kept, so the longest
    // beginning that fits is found by halving.
    let (mut fits, mut over) = (0, ends.len());
    while fits + 1 < over {
        let mid = (fits + over) / 2;
        if label_width(&cut(mid), m) <= max_w {
            fits = mid;
        } else {
            over = mid;
        }
    }
    cut(fits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Six px per character at any size.
    fn fake(s: &str, _pt: f64, _weight: u16) -> f64 {
        s.chars().count() as f64 * 6.0
    }

    fn modal(window: (f64, f64), cols: usize, rows: usize, label_w: f64) -> LayoutModal {
        LayoutModal::layout(window.0, window.1, 0.0, cols, rows, label_w, &mut fake)
    }

    const ROOMY: (f64, f64) = (3840.0, 2160.0);

    fn centre(r: Rect) -> (f64, f64) {
        (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
    }

    fn same(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn layout_modal_is_centred_in_the_window() {
        let m = modal(ROOMY, 3, 3, 0.0);
        let (cx, cy) = centre(m.chrome.frame);
        assert!(same(cx, ROOMY.0 * 0.5) && same(cy, ROOMY.1 * 0.5));
    }

    /// The close button reacts where the shared chrome draws it, and
    /// nowhere else.
    #[test]
    fn layout_modal_close_hit_box_is_the_one_chrome_draws() {
        let m = modal(ROOMY, 4, 1, 0.0);
        let drawn = PanelChrome::new(m.chrome.frame, 0.0).close;
        assert_eq!(m.chrome.close, drawn);
        let (x, y) = centre(drawn);
        assert_eq!(m.hover_at(x, y), Hover::Close);
        assert_eq!(m.hover_at(drawn.x + 1.0, drawn.y_top + 1.0), Hover::Close);
        assert_eq!(m.hover_at(drawn.x - 1.0, y), Hover::None, "left of it is the header");
        assert_eq!(m.hover_at(x, drawn.y_top + drawn.h + 1.0), Hover::None);
    }

    /// A stepper is clickable on the rects `stepper_rects` gives the
    /// painter for it: right-aligned in its row, centred in the band.
    #[test]
    fn layout_modal_stepper_hit_boxes_are_the_stepper_rects() {
        let m = modal(ROOMY, 4, 1, 0.0);
        let (w, h) = controls::stepper_size();
        for (n, axis) in [Axis::Columns, Axis::Rows].into_iter().enumerate() {
            let row = m.rows[n];
            assert_eq!(row.axis, axis);
            assert!(same(row.band.y_top, m.grid_card.y_top + n as f64 * spec::px(metric::ROW_H)));
            let x = m.grid_card.x + m.grid_card.w - spec::px(card::ROW_PAD_X) - w;
            let y = row.band.y_top + (row.band.h - h) * 0.5;
            let drawn = controls::stepper_rects(x, y);
            assert_eq!(
                (row.stepper.minus, row.stepper.value, row.stepper.plus),
                (drawn.minus, drawn.value, drawn.plus),
            );
            let (mx, my) = centre(drawn.minus);
            assert_eq!(m.hover_at(mx, my), Hover::Step(axis, StepperPart::Minus));
            let (px, py) = centre(drawn.plus);
            assert_eq!(m.hover_at(px, py), Hover::Step(axis, StepperPart::Plus));
            let (vx, vy) = centre(drawn.value);
            assert_eq!(m.hover_at(vx, vy), Hover::None, "the value is not a button");
        }
        let last = m.rows[1].band;
        assert!(same(last.y_top + last.h, m.grid_card.y_top + m.grid_card.h), "two rows fill the card");
    }

    #[test]
    fn layout_modal_steps_stop_at_both_ends_of_the_range() {
        assert_eq!(step(4, StepperPart::Minus), Some(3));
        assert_eq!(step(4, StepperPart::Plus), Some(5));
        assert_eq!(step(GRID_MIN, StepperPart::Minus), None);
        assert_eq!(step(GRID_MAX, StepperPart::Plus), None);
        assert_eq!(step(GRID_MIN, StepperPart::Plus), Some(GRID_MIN + 1));
        assert_eq!(step(GRID_MAX, StepperPart::Minus), Some(GRID_MAX - 1));
    }

    /// One primary button, at the right of the footer, as wide as a
    /// button is — not stretched across the row.
    #[test]
    fn layout_modal_apply_sits_at_the_right_of_the_footer() {
        let m = modal(ROOMY, 4, 1, 0.0);
        let c = m.chrome.content;
        let (w, h) = controls::button_size(t(APPLY), &mut fake);
        assert_eq!((m.apply_btn.w, m.apply_btn.h), (w, h));
        assert!(m.apply_btn.w < c.w * 0.5);
        assert!(same(m.apply_btn.x + m.apply_btn.w, c.x + c.w));
        assert!(same(m.apply_btn.y_top + m.apply_btn.h, c.y_top + c.h));
        assert_eq!((m.footer.x, m.footer.y_top), (c.x, m.apply_btn.y_top));
        let below_cards = m.footer.y_top - (m.card_grid.y_top + m.card_grid.h);
        assert!(same(below_cards, spec::px(space::CARD_TO_FOOTER)));
        let (x, y) = centre(m.apply_btn);
        assert_eq!(m.hover_at(x, y), Hover::Apply);
        assert_eq!(m.hover_at(c.x + 1.0, y), Hover::None, "the caption is not a button");
    }

    #[test]
    fn layout_modal_cards_and_the_outside_hit_apart() {
        let m = modal(ROOMY, 3, 2, 0.0);
        assert_eq!(m.cards.len(), 6);
        for (slot, r) in m.cards.iter().enumerate() {
            let (x, y) = centre(*r);
            assert_eq!(m.hover_at(x, y), Hover::Card(slot));
            assert!(m.contains(x, y));
        }
        let between = m.cards[0].x + m.cards[0].w + spec::px(metric::CARD_GAP) * 0.5;
        assert_eq!(m.hover_at(between, centre(m.cards[0]).1), Hover::None);
        assert_eq!(m.hover_at(1.0, 1.0), Hover::None);
        assert!(!m.contains(1.0, 1.0), "a click out here closes the panel");
    }

    /// The groups are spaced by the shared gaps, top to bottom.
    #[test]
    fn layout_modal_groups_are_spaced_by_the_spec() {
        let m = modal(ROOMY, 4, 2, 0.0);
        let c = m.chrome.content;
        let heading = panel_chrome::heading_height();
        assert_eq!(m.grid_heading_top, c.y_top);
        assert!(same(m.grid_card.y_top, m.grid_heading_top + heading));
        assert_eq!((m.grid_card.x, m.grid_card.w), (c.x, c.w));
        assert!(same(
            m.arrangement_heading_top - (m.grid_card.y_top + m.grid_card.h),
            spec::px(space::CARD_TO_HEADING),
        ));
        assert!(same(m.card_grid.y_top, m.arrangement_heading_top + heading));
        assert!(same(m.cards[0].h, spec::px(metric::ROW_H)), "with room, a card is one row tall");
        let gap = spec::px(metric::CARD_GAP);
        assert!(same(m.cards[1].x - (m.cards[0].x + m.cards[0].w), gap));
        assert!(same(m.cards[4].y_top - (m.cards[0].y_top + m.cards[0].h), gap));
    }

    /// Cards stay inside the content area at every shape, label length
    /// and window size — nine columns included.
    #[test]
    fn layout_modal_cards_never_leave_the_content_area() {
        for window in [ROOMY, (2160.0, 1300.0), (1800.0, 1400.0), (1200.0, 1000.0)] {
            for cols in GRID_MIN..=GRID_MAX {
                for rows in GRID_MIN..=GRID_MAX {
                    for label_w in [60.0, 0.0, 202.0, 800.0] {
                        let m = modal(window, cols, rows, label_w);
                        let (f, c) = (m.chrome.frame, m.chrome.content);
                        let at = format!("{cols}x{rows} in {window:?}, label {label_w}");
                        assert!(f.w <= window.0 && f.h <= window.1, "{at}: larger than the window");
                        assert_eq!(m.cards.len(), cols * rows);
                        for (i, r) in m.cards.iter().enumerate() {
                            assert!(
                                r.x >= c.x - 1e-6 && r.x + r.w <= c.x + c.w + 1e-6,
                                "{at}: card {i} spans {}..{} outside {}..{}",
                                r.x, r.x + r.w, c.x, c.x + c.w,
                            );
                            assert!(
                                r.y_top >= m.card_grid.y_top - 1e-6
                                    && r.y_top + r.h <= m.footer.y_top + 1e-6,
                                "{at}: card {i} runs into the footer",
                            );
                            assert!(r.w > 0.0 && r.h > 0.0, "{at}: card {i} has no area");
                        }
                        assert!(m.footer.y_top + m.footer.h <= c.y_top + c.h + 1e-6);
                    }
                }
            }
        }
        // The widest grid in the widest panel is exactly the one whose
        // cards are still the least width.
        let m = modal(ROOMY, GRID_MAX, 1, 0.0);
        assert_eq!(m.chrome.frame.w, spec::px(metric::WIDEST));
        assert!(same(m.cards[0].w, spec::px(metric::CARD_MIN_W)));
        let last = m.cards[GRID_MAX - 1];
        let c = m.chrome.content;
        assert!(same(last.x + last.w, c.x + c.w), "the row ends on the content's edge");
    }

    /// A window too short for nine rows squeezes the cards, not the
    /// controls, and the last row stays clear of the footer.
    #[test]
    fn layout_modal_short_window_squeezes_only_the_cards() {
        let roomy = modal(ROOMY, 3, GRID_MAX, 0.0);
        let short = modal((1800.0, 1100.0), 3, GRID_MAX, 0.0);
        assert!(short.cards[0].h < roomy.cards[0].h);
        assert_eq!(short.grid_card.h, roomy.grid_card.h);
        assert_eq!(short.apply_btn.h, roomy.apply_btn.h);
        let last = short.cards.last().unwrap();
        let below = short.footer.y_top - (last.y_top + last.h);
        assert!(same(below, spec::px(space::CARD_TO_FOOTER)));
    }

    /// The panel is one of its fixed widths, the narrowest its cards
    /// fit: more columns or a longer name take the next one up.
    #[test]
    fn layout_modal_takes_the_narrowest_width_its_cards_fit() {
        let step_of = |cols: usize, label_w: f64| modal(ROOMY, cols, 1, label_w).chrome.frame.w;
        let [narrow, medium, wide] = metric::WIDTHS.map(spec::px);
        assert_eq!(step_of(1, 0.0), narrow);
        assert_eq!(step_of(4, 0.0), narrow);
        assert_eq!(step_of(GRID_MAX, 0.0), wide);
        assert!(step_of(6, 0.0) > narrow);
        // A name as wide as a least-width card needs its padding too.
        assert_eq!(step_of(4, spec::px(metric::CARD_MIN_W)), medium);
        let a = modal(ROOMY, 3, 2, 40.0);
        let b = modal(ROOMY, 3, 2, 600.0);
        assert!(b.cards[0].w > a.cards[0].w, "a longer name gets a wider card");
        // No name makes the panel wider than its widest step, or than
        // the window allows.
        assert_eq!(step_of(3, 5000.0), wide);
        let capped = modal((1000.0, 1400.0), 6, 2, 600.0);
        assert!(capped.chrome.frame.w <= 1000.0 * spec::panel::MAX_W_FRAC + 1e-9);
    }

    #[test]
    fn layout_modal_long_name_is_cut_to_its_card() {
        let name = "lab38-golialab-experiments-with-a-very-long-directory-name";
        let m = modal(ROOMY, GRID_MAX, 1, fake(name, 0.0, 0));
        let room = label_room(m.cards[0].w);
        assert!(fake(name, 0.0, 0) > room, "the name does not fit a card of nine");
        let cut = fit_label(name, room, &mut fake);
        assert!(cut.ends_with('…'), "a cut has to show: {cut:?}");
        assert!(fake(&cut, 0.0, 0) <= room, "{cut:?} still overflows {room}");
        let kept = cut.trim_end_matches('…');
        assert!(name.starts_with(kept) && !kept.is_empty());
        // The longest beginning that fits: one more character would not.
        let more: String = name.chars().take(kept.chars().count() + 1).chain(['…']).collect();
        assert!(fake(&more, 0.0, 0) > room);
        // What fits is left alone, and no room at all is an ellipsis.
        assert_eq!(fit_label("smix", room, &mut fake), "smix");
        assert_eq!(fit_label("", room, &mut fake), "");
        assert_eq!(fit_label(name, 0.0, &mut fake), "…");
        assert_eq!(fit_label("窗格的名字很长很长很长很长很长", 40.0, &mut fake), "窗格的名字…");
        let titles = ["smix".to_string(), name.to_string(), String::new()];
        assert_eq!(widest_label(&titles, &mut fake), fake(name, 0.0, 0));
        assert_eq!(widest_label(&[], &mut fake), 0.0);
    }

    #[test]
    fn layout_modal_drop_target_follows_the_dragged_cards_centre() {
        let m = modal(ROOMY, 4, 1, 0.0);
        let grab = (5.0, 5.0);
        let held_at = |slot: usize| (m.cards[slot].x + grab.0, m.cards[slot].y_top + grab.1);
        assert_eq!(m.drop_target(0, held_at(0), grab), None, "still over its own slot");
        assert_eq!(m.drop_target(0, held_at(2), grab), Some(2));
        assert_eq!(m.drop_target(3, held_at(1), grab), Some(1));
        // Far outside the panel the nearest slot still takes it.
        assert_eq!(m.drop_target(0, ROOMY, grab), Some(3));
        assert_eq!(m.drop_target(9, held_at(1), grab), None, "no such slot");
        assert_eq!(m.nearest_card(m.cards[1].x, m.cards[1].y_top), Some(1));
    }

    #[test]
    fn layout_modal_caption_counts_the_panes() {
        assert_eq!(caption(4, 1), "4 × 1 = 4 panes");
        assert_eq!(caption(3, 3), "3 × 3 = 9 panes");
        assert_eq!(caption(1, 1), "1 × 1 = 1 pane");
    }
}

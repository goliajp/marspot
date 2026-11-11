//! Where everything in the agent-accounts panel goes.
//!
//! Pure geometry, no painter: the panel's height, the click handler
//! and the painter all ask here, so the frame is exactly as tall as
//! what is drawn in it and nothing is drawn outside a card.
//!
//! Top to bottom inside the chrome's content rect: the `Accounts`
//! heading, the account cards (one row, or as many as it takes for no
//! card to be narrower than `metric::CARD_MIN_W`), the `Availability`
//! heading, the timeline in one card.
//!
//! All rects are physical pixels.

use marspot_term::layout::Rect;

use crate::ui::components::cc_usage_modal::{fit_ellipsis, metric, CcUsageAccountRender};
use crate::ui::components::panel_chrome;
use crate::ui::theme::spec::{self, control, space, table};

/// What a rounding error may cost before a row is called not fitting.
const SLACK: f64 = 1e-6;

/// The terminal font's cell, which the panel's data is set in.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub w: f64,
    pub h: f64,
    pub ascent: f64,
}

impl Cell {
    /// Baseline of one line of the terminal font centred in a box.
    pub fn baseline_in(&self, top: f64, h: f64) -> f64 {
        top + (h - self.h) * 0.5 + self.ascent
    }

    pub fn text_w(&self, s: &str) -> f64 {
        crate::ui::core::text_fit::cells(s) as f64 * self.w
    }
}

/// The few things about the accounts that decide where things go.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    pub accounts: usize,
    /// Most windows any account is metered on: every card and every
    /// timeline row is sized for it, because rows of different heights
    /// read as different kinds of thing.
    pub window_rows: usize,
    /// Longest account name, in characters.
    pub name_cols: usize,
    /// Longest tag right of a timeline bar, in characters.
    pub tag_cols: usize,
}

impl Shape {
    pub fn of(accounts: &[CcUsageAccountRender]) -> Self {
        let windows = || accounts.iter().flat_map(|a| a.windows.iter());
        Self {
            accounts: accounts.len(),
            window_rows: accounts.iter().map(|a| a.windows.len()).max().unwrap_or(0),
            name_cols: accounts.iter().map(|a| a.name.chars().count()).max().unwrap_or(0),
            tag_cols: windows().map(|w| w.tag().chars().count()).max().unwrap_or(0),
        }
    }
}

/// How many cards go in one row of a `content_w` wide content area.
///
/// As many as fit at the least width — then spread evenly over the
/// rows that takes, so ten accounts wrap to five and five rather than
/// seven and three.
pub fn card_columns(content_w: f64, n: usize) -> usize {
    let n = n.max(1);
    let gap = spec::px(space::CARD_TO_CARD);
    let fit = (((content_w + gap) / (spec::px(metric::CARD_MIN_W) + gap)).floor() as usize).max(1);
    n.div_ceil(n.div_ceil(fit))
}

/// Height of an account card whose accounts are metered on up to
/// `window_rows` windows.
pub fn card_height(cell_h: f64, window_rows: usize) -> f64 {
    spec::px(
        metric::CARD_PAD
            + control::CHIP_H
            + metric::NAME_TO_EMAIL
            + metric::EMAIL.cap()
            + metric::EMAIL.descent()
            + metric::EMAIL_TO_BARS
            + metric::BARS_TO_RESET
            + metric::RESET.cap()
            + metric::RESET.descent()
            + metric::CARD_PAD,
    ) + table::row_h(cell_h) * window_rows as f64
}

/// Height of `n` stacked timeline bars including the gaps between them.
pub fn bars_height(cell_h: f64, n: usize) -> f64 {
    let n = n.max(1) as f64;
    cell_h * (metric::BAR_H_CELLS * n + metric::TIMELINE_BAR_GAP_CELLS * (n - 1.0))
}

/// Height of one timeline row: its bars, and a table row's worth of
/// air shared between the space above and below them.
pub fn timeline_row_height(cell_h: f64, bars: usize) -> f64 {
    bars_height(cell_h, bars) + table::row_h(cell_h)
}

/// What the timeline card holds besides its rows: its padding, the
/// `NOW` line above the plot and the dates below it.
fn timeline_fixed_height(cell_h: f64) -> f64 {
    2.0 * (spec::px(metric::CARD_PAD + metric::PLOT_TO_AXIS) + cell_h)
}

/// Height of everything under the header for `n` accounts.
pub fn content_height(content_w: f64, cell_h: f64, n: usize, window_rows: usize) -> f64 {
    if n == 0 {
        return spec::px(metric::EMPTY_H);
    }
    let card_rows = n.div_ceil(card_columns(content_w, n)) as f64;
    panel_chrome::heading_height()
        + card_rows * card_height(cell_h, window_rows)
        + (card_rows - 1.0) * spec::px(space::CARD_TO_CARD)
        + spec::px(space::CARD_TO_HEADING)
        + panel_chrome::heading_height()
        + timeline_fixed_height(cell_h)
        + n as f64 * timeline_row_height(cell_h, window_rows)
}

/// Where the parts of the panel's content are.
#[derive(Debug, Clone)]
pub struct UsageLayout {
    /// Top of the `Accounts` heading's line.
    pub heading_top: f64,
    /// One per account, in order.  Shorter than the account list when
    /// the window is too short for every row of cards.
    pub cards: Vec<Rect>,
    /// `None` when the window is too short for even one of its rows.
    pub timeline: Option<Timeline>,
}

/// The parts of `content` for accounts of `shape`.
pub fn layout(content: Rect, cell: Cell, shape: &Shape) -> UsageLayout {
    let bottom = content.y_top + content.h + SLACK;
    let gap = spec::px(space::CARD_TO_CARD);
    let cols = card_columns(content.w, shape.accounts);
    let card_w = (content.w - gap * (cols as f64 - 1.0)) / cols as f64;
    let card_h = card_height(cell.h, shape.window_rows);
    let cards_top = content.y_top + panel_chrome::heading_height();
    let card_at = |i: usize| Rect {
        x: content.x + (i % cols) as f64 * (card_w + gap),
        y_top: cards_top + (i / cols) as f64 * (card_h + gap),
        w: card_w,
        h: card_h,
    };
    let card_rows = shape.accounts.max(1).div_ceil(cols) as f64;
    let cards_bottom = cards_top + card_rows * card_h + (card_rows - 1.0) * gap;
    let timeline_top = cards_bottom + spec::px(space::CARD_TO_HEADING);
    UsageLayout {
        heading_top: content.y_top,
        // Nothing clips the content, so a card that would cross the
        // frame's bottom edge is left out whole.
        cards: (0..shape.accounts).map(card_at).filter(|c| c.y_top + c.h <= bottom).collect(),
        timeline: Timeline::new(content, timeline_top, cell, shape),
    }
}

/// The inside of one account card.
#[derive(Debug, Clone, Copy)]
pub struct CardRows {
    /// The card's inner left and right edges.
    pub left: f64,
    pub right: f64,
    /// The band the account name and the status chip share, as tall as
    /// the chip.
    pub name_line: Rect,
    pub email_baseline: f64,
    /// Top of the first usage bar's row; each row is `bar_row_h` tall.
    pub bars_top: f64,
    pub bar_row_h: f64,
    pub reset_baseline: f64,
}

/// One usage bar's row in a card: `7d  [======----]  55%`.
#[derive(Debug, Clone, Copy)]
pub struct BarRow {
    /// The row's box; label and percentage are centred in it.
    pub top: f64,
    pub h: f64,
    pub bar: Rect,
    /// The percentage is right-aligned to the card's inner edge.
    pub pct_right: f64,
}

/// The inside of `card`, whose accounts are metered on up to
/// `window_rows` windows.
pub fn card_rows(card: Rect, cell_h: f64, window_rows: usize) -> CardRows {
    let pad = spec::px(metric::CARD_PAD);
    let (left, right) = (card.x + pad, card.x + card.w - pad);
    let name_line = Rect {
        x: left,
        y_top: card.y_top + pad,
        w: (right - left).max(0.0),
        h: spec::px(control::CHIP_H),
    };
    let email_baseline =
        name_line.y_top + name_line.h + spec::px(metric::NAME_TO_EMAIL + metric::EMAIL.cap());
    let bars_top = email_baseline + spec::px(metric::EMAIL.descent() + metric::EMAIL_TO_BARS);
    let bar_row_h = table::row_h(cell_h);
    // Under the tallest card's last bar in every card, not under this
    // account's own: the reset lines of a row of cards then share one
    // baseline.
    let reset_baseline = bars_top
        + bar_row_h * window_rows as f64
        + spec::px(metric::BARS_TO_RESET + metric::RESET.cap());
    CardRows { left, right, name_line, email_baseline, bars_top, bar_row_h, reset_baseline }
}

impl CardRows {
    /// The status chip of size `(w, h)`: at the right end of the name's
    /// line, never wider than the line.
    pub fn chip(&self, (w, h): (f64, f64)) -> Rect {
        let w = w.min(self.name_line.w);
        Rect {
            x: self.right - w,
            y_top: self.name_line.y_top + (self.name_line.h - h) * 0.5,
            w,
            h,
        }
    }

    /// The `i`th usage bar's row.  `label_cols` is the widest window
    /// label of the card, so its bars all start at one x.
    pub fn bar_row(&self, i: usize, label_cols: usize, cell: Cell) -> BarRow {
        let top = self.bars_top + self.bar_row_h * i as f64;
        let bar_h = cell.h * metric::BAR_H_CELLS;
        let x = self.left + (label_cols as f64 + metric::BAR_GAP_CELLS) * cell.w;
        let end = self.right - (metric::PCT_CELLS + metric::BAR_GAP_CELLS) * cell.w;
        BarRow {
            top,
            h: self.bar_row_h,
            bar: Rect {
                x,
                y_top: top + (self.bar_row_h - bar_h) * 0.5,
                w: (end - x).max(0.0),
                h: bar_h,
            },
            pct_right: self.right,
        }
    }
}

/// The timeline card: account names down the left, one row of bars
/// per account on a shared time axis, dates along the bottom.
#[derive(Debug, Clone, Copy)]
pub struct Timeline {
    /// Top of the `Availability` heading's line.
    pub heading_top: f64,
    pub card: Rect,
    /// The card's inner left and right edges.  Nothing is drawn
    /// outside them.
    pub left: f64,
    pub right: f64,
    /// Characters an account name has room for.
    pub name_cols: f64,
    /// The time axis: `[plot_x, plot_x + plot_w]`.
    pub plot_x: f64,
    pub plot_w: f64,
    /// Top of the line the `NOW` label is on.
    pub now_top: f64,
    pub rows_top: f64,
    pub row_h: f64,
    /// Rows drawn.  Fewer than there are accounts when the window is
    /// too short for all of them.
    pub rows: usize,
    /// Top of the line the dates are on.
    pub axis_top: f64,
}

impl Timeline {
    fn new(content: Rect, heading_top: f64, cell: Cell, shape: &Shape) -> Option<Self> {
        let card_top = heading_top + panel_chrome::heading_height();
        let row_h = timeline_row_height(cell.h, shape.window_rows);
        let fixed = timeline_fixed_height(cell.h);
        let room = content.y_top + content.h - card_top - fixed + SLACK;
        let rows = shape.accounts.min((room / row_h).floor().max(0.0) as usize);
        if rows == 0 {
            return None;
        }
        let pad = spec::px(metric::CARD_PAD);
        let card = Rect { x: content.x, y_top: card_top, w: content.w, h: fixed + rows as f64 * row_h };
        let (left, right) = (card.x + pad, card.x + card.w - pad);
        // The names and the tags each give way before the plot does.
        let side_max = (right - left) * metric::TIMELINE_SIDE_MAX_FRAC;
        let names_w = ((shape.name_cols as f64 + table::COLUMN_GAP_CELLS) * cell.w).min(side_max);
        // Room right of the axis for the tag of a bar that ends at its
        // very end.  A tag that still does not fit is cut, see
        // [`Timeline::tag`].
        let tags_w = ((shape.tag_cols as f64 + metric::BAR_GAP_CELLS) * cell.w).min(side_max);
        let now_top = card.y_top + pad;
        let rows_top = now_top + cell.h + spec::px(metric::PLOT_TO_AXIS);
        Some(Self {
            heading_top,
            card,
            left,
            right,
            name_cols: (names_w / cell.w - table::COLUMN_GAP_CELLS).max(0.0),
            plot_x: left + names_w,
            plot_w: (right - tags_w - left - names_w).max(cell.w),
            now_top,
            rows_top,
            row_h,
            rows,
            axis_top: rows_top + rows as f64 * row_h + spec::px(metric::PLOT_TO_AXIS),
        })
    }

    pub fn rows_bottom(&self) -> f64 {
        self.rows_top + self.rows as f64 * self.row_h
    }

    pub fn row_top(&self, row: usize) -> f64 {
        self.rows_top + row as f64 * self.row_h
    }

    /// The x of the point `frac` of the way along the time axis.
    pub fn x_of(&self, frac: f64) -> f64 {
        self.plot_x + frac.clamp(0.0, 1.0) * self.plot_w
    }

    /// The `idx`th of `own_bars` bars of `row`, from `f0` to `f1` of
    /// the time axis.  An account's bars are centred in its row as a
    /// stack, however many it has.
    pub fn bar(&self, row: usize, idx: usize, own_bars: usize, cell_h: f64, f0: f64, f1: f64) -> Rect {
        let stack_top = self.row_top(row) + (self.row_h - bars_height(cell_h, own_bars)) * 0.5;
        let pitch = cell_h * (metric::BAR_H_CELLS + metric::TIMELINE_BAR_GAP_CELLS);
        let (x0, x1) = (self.x_of(f0), self.x_of(f1));
        Rect {
            x: x0,
            y_top: stack_top + idx as f64 * pitch,
            w: (x1 - x0).max(0.0),
            h: cell_h * metric::BAR_H_CELLS,
        }
    }

    /// Where the tag of a bar ending at `bar_end` starts, and as much
    /// of it as fits before the card's inner edge — nothing, when that
    /// is too little to read.
    pub fn tag(&self, tag: &str, bar_end: f64, cell_w: f64) -> (f64, String) {
        let x = bar_end + metric::BAR_GAP_CELLS * cell_w;
        (x, fit_ellipsis(tag, (self.right - x) / cell_w))
    }

    /// Left edge of a `w` wide label centred on `x`, or `None` when it
    /// would cross either inner edge of the card.
    pub fn centred_label_x(&self, x: f64, w: f64) -> Option<f64> {
        let lx = x - w * 0.5;
        (lx >= self.left && lx + w <= self.right).then_some(lx)
    }

    /// Every how many days a date is written, for days `day_w` apart:
    /// each one, unless they would run into each other.
    pub fn axis_stride(day_w: f64, cell_w: f64) -> usize {
        if day_w <= 0.0 {
            return 1;
        }
        ((metric::AXIS_LABEL_CELLS * cell_w / day_w).ceil() as usize).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::components::cc_usage_modal::{panel_rect, CcUsageWindowRender};
    use crate::ui::components::panel_chrome::PanelChrome;

    const CELL: Cell = Cell { w: 8.0, h: 16.0, ascent: 12.0 };

    fn shape(accounts: usize, window_rows: usize) -> Shape {
        Shape { accounts, window_rows, name_cols: 8, tag_cols: 16 }
    }

    /// The content rect of the panel in a `w × h` window.
    fn content(w: f64, h: f64, s: &Shape) -> Rect {
        let rect = panel_rect(s.accounts, s.window_rows, w, h, CELL.w, CELL.h, 30.0);
        PanelChrome::new(rect, 0.0).content
    }

    fn inside(inner: Rect, outer: Rect) -> bool {
        inner.x >= outer.x - SLACK
            && inner.y_top >= outer.y_top - SLACK
            && inner.x + inner.w <= outer.x + outer.w + SLACK
            && inner.y_top + inner.h <= outer.y_top + outer.h + SLACK
    }

    /// Ten accounts are one row in a wide window and two rows of five
    /// in a narrower one; either way the cards are equal, a gap apart,
    /// and fill the content's width.
    #[test]
    fn cards_wrap_when_one_row_would_make_them_too_narrow() {
        let s = shape(10, 2);
        let gap = spec::px(space::CARD_TO_CARD);
        let min_w = spec::px(metric::CARD_MIN_W);
        for (window_w, cols, rows) in [(2800.0, 10usize, 1usize), (1700.0, 5, 2), (700.0, 2, 5)] {
            let c = content(window_w, 4000.0, &s);
            assert_eq!(card_columns(c.w, 10), cols, "window {window_w}");
            let l = layout(c, CELL, &s);
            assert_eq!(l.cards.len(), 10, "window {window_w}: every account has a card");
            let first = l.cards[0];
            assert!(first.w >= min_w, "window {window_w}: card is {} wide", first.w);
            for (i, card) in l.cards.iter().enumerate() {
                assert_eq!((card.w, card.h), (first.w, first.h), "card {i} is the size of the first");
                let (col, row) = (i % cols, i / cols);
                assert!((card.x - (c.x + col as f64 * (first.w + gap))).abs() < SLACK);
                assert!((card.y_top - (first.y_top + row as f64 * (first.h + gap))).abs() < SLACK);
                assert!(inside(*card, c), "window {window_w}: card {i} leaves the content area");
            }
            assert_eq!(l.cards[9].y_top, first.y_top + (rows - 1) as f64 * (first.h + gap));
            let row_right = l.cards[cols - 1].x + first.w;
            assert!((row_right - (c.x + c.w)).abs() < SLACK, "a row ends on the content's right edge");
            assert_eq!(first.y_top, c.y_top + panel_chrome::heading_height());
        }
        // Six cards of the least width fit this one and seven do not,
        // which is why ten do not go in one row.
        let c = content(1700.0, 4000.0, &s);
        assert!((c.w - 5.0 * gap) / 6.0 >= min_w && (c.w - 6.0 * gap) / 7.0 < min_w);
        // Fewer accounts than fit: one row, still filling the width.
        assert_eq!(card_columns(c.w, 3), 3);
        assert_eq!(card_columns(c.w, 0), 1);
    }

    /// The frame is as tall as what is drawn in it: the timeline card
    /// ends on the content's bottom edge, to within the pixel the frame
    /// is rounded up to.
    #[test]
    fn the_panel_is_as_tall_as_its_content() {
        for (n, window_rows, window_w) in [(10, 2, 2800.0), (10, 2, 1700.0), (2, 4, 1700.0), (1, 1, 900.0)] {
            let s = shape(n, window_rows);
            let c = content(window_w, 6000.0, &s);
            let l = layout(c, CELL, &s);
            let t = l.timeline.expect("room for the timeline");
            assert_eq!(t.rows, n);
            assert!(
                (0.0..1.0).contains(&(c.y_top + c.h - (t.card.y_top + t.card.h))),
                "n={n} w={window_w}: card ends at {}, content at {}",
                t.card.y_top + t.card.h,
                c.y_top + c.h,
            );
            let last = l.cards[n - 1];
            assert!(
                (t.heading_top - (last.y_top + last.h + spec::px(space::CARD_TO_HEADING))).abs() < SLACK,
                "the second heading is one heading gap under the cards",
            );
        }
    }

    /// Every line of a card is inside it, with the same inset on all
    /// four sides.
    #[test]
    fn a_cards_rows_keep_the_cards_padding() {
        let pad = spec::px(metric::CARD_PAD);
        for window_rows in [1usize, 2, 4] {
            let card = Rect { x: 40.0, y_top: 100.0, w: 260.0, h: card_height(CELL.h, window_rows) };
            let r = card_rows(card, CELL.h, window_rows);
            assert_eq!((r.left - card.x, card.x + card.w - r.right), (pad, pad));
            assert_eq!(r.name_line.y_top - card.y_top, pad);
            let reset_bottom = r.reset_baseline + spec::px(metric::RESET.descent());
            assert!(
                (card.y_top + card.h - reset_bottom - pad).abs() < SLACK,
                "window_rows={window_rows}: {} under the last line",
                card.y_top + card.h - reset_bottom,
            );
            // The lines follow one another without overlapping.
            let email_top = r.email_baseline - spec::px(metric::EMAIL.cap());
            assert!(email_top >= r.name_line.y_top + r.name_line.h);
            assert!(r.bars_top > r.email_baseline);
            let chip = r.chip((70.0, spec::px(control::CHIP_H)));
            assert_eq!(chip.x + chip.w, r.right, "the chip ends on the inner edge");
            assert!(inside(chip, r.name_line));
            assert_eq!(r.chip((9999.0, 10.0)).x, r.left, "a chip wider than the card is cut to it");
            for i in 0..window_rows {
                let row = r.bar_row(i, 5, CELL);
                assert!(row.bar.x >= r.left + 5.0 * CELL.w, "bar {i} starts after its label");
                assert!(row.bar.x + row.bar.w <= row.pct_right - metric::PCT_CELLS * CELL.w);
                assert!(row.bar.y_top >= row.top && row.bar.y_top + row.bar.h <= row.top + row.h);
                assert!(row.top + row.h <= r.reset_baseline - spec::px(metric::RESET.cap()));
            }
            // A card too narrow for a bar draws none, not a negative one.
            let narrow = card_rows(Rect { w: 60.0, ..card }, CELL.h, window_rows);
            assert_eq!(narrow.bar_row(0, 6, CELL).bar.w, 0.0);
        }
    }

    /// An address longer than its card is cut to the card's inner
    /// width, and the cut is marked.
    #[test]
    fn a_long_email_is_cut_inside_its_card() {
        use crate::ui::components::cc_usage_modal::fit_measured;
        let s = shape(10, 2);
        let l = layout(content(1700.0, 4000.0, &s), CELL, &s);
        let r = card_rows(l.cards[3], CELL.h, s.window_rows);
        let mut measure = |t: &str| t.chars().count() as f64 * 13.0;
        let email = "a.remarkably.long.mailbox.name.for.an.account@subdomain.example.com";
        assert!(measure(email) > r.right - r.left, "the address must not fit for this to mean anything");
        let shown = fit_measured(email, r.right - r.left, &mut measure);
        assert!(shown.ends_with('…') && shown.chars().count() > 4, "{shown:?}");
        assert!(r.left + measure(&shown) <= r.right, "{shown:?} crosses the card's padding");
        // A short one is left as it is.
        assert_eq!(fit_measured("a@b.co", r.right - r.left, &mut measure), "a@b.co");
    }

    fn window(reset_hm: &str) -> CcUsageWindowRender {
        CcUsageWindowRender {
            label: "fable".to_string(),
            util: 1.0,
            reset_unix: None,
            span_secs: 0,
            reset_hm: reset_hm.to_string(),
        }
    }

    /// Rows, bars and tags all stay between the timeline card's inner
    /// edges — also when the window is too short for every account,
    /// and when a bar ends at the very end of the axis.
    #[test]
    fn timeline_rows_stay_inside_the_card() {
        let tag = window("23:59").tag();
        assert_eq!(tag, "fable 100% 23:59");
        let pad = spec::px(metric::CARD_PAD);
        let mut cut_short = 0;
        for (n, window_rows, window_w, window_h) in [
            (10usize, 2usize, 2800.0, 1300.0),
            (10, 2, 1700.0, 1500.0),
            (10, 2, 1700.0, 900.0),
            (14, 4, 1200.0, 1000.0),
            (3, 1, 500.0, 700.0),
        ] {
            let s = Shape { accounts: n, window_rows, name_cols: 9, tag_cols: tag.chars().count() };
            let c = content(window_w, window_h, &s);
            let l = layout(c, CELL, &s);
            let Some(t) = l.timeline else {
                // No room for a single row: then there is no card either.
                cut_short += 1;
                continue;
            };
            let case = format!("n={n} {window_w}x{window_h}");
            assert!(inside(t.card, c), "{case}: the card leaves the content area");
            assert_eq!((t.left - t.card.x, t.card.x + t.card.w - t.right), (pad, pad), "{case}");
            assert!(t.rows >= 1 && t.rows <= n, "{case}");
            cut_short += (t.rows < n) as usize;
            assert_eq!(t.now_top - t.card.y_top, pad, "{case}");
            assert!(
                (t.card.y_top + t.card.h - (t.axis_top + CELL.h) - pad).abs() < SLACK,
                "{case}: same inset under the dates as above NOW",
            );
            assert!(t.rows_top >= t.now_top + CELL.h && t.axis_top >= t.rows_bottom(), "{case}");
            assert!(t.plot_x >= t.left && t.plot_x + t.plot_w <= t.right + SLACK, "{case}");
            for row in 0..t.rows {
                for idx in 0..window_rows {
                    // The longest bar there can be: the whole axis.
                    let bar = t.bar(row, idx, window_rows, CELL.h, 0.0, 1.0);
                    assert!(bar.x >= t.plot_x && bar.x + bar.w <= t.right + SLACK, "{case} row {row}");
                    assert!(
                        bar.y_top >= t.row_top(row) && bar.y_top + bar.h <= t.row_top(row) + t.row_h,
                        "{case}: bar {idx} leaves row {row}",
                    );
                    for end in [bar.x + bar.w, bar.x + bar.w * 0.5, t.right - 2.0 * CELL.w] {
                        let (x, shown) = t.tag(&tag, end, CELL.w);
                        assert!(
                            x + CELL.text_w(&shown) <= t.right + SLACK,
                            "{case}: tag {shown:?} at {x} crosses the card's padding at {}",
                            t.right,
                        );
                        assert!(shown == tag || shown.is_empty() || shown.ends_with('…'));
                    }
                }
                assert!(t.row_top(row) + t.row_h <= t.rows_bottom() + SLACK);
            }
            // A bar that ends where the axis does still has its whole
            // tag, as long as the card is wide enough to spare the room.
            if (s.tag_cols as f64 + metric::BAR_GAP_CELLS) * CELL.w
                <= (t.right - t.left) * metric::TIMELINE_SIDE_MAX_FRAC
            {
                assert_eq!(t.tag(&tag, t.x_of(1.0), CELL.w).1, tag, "{case}");
            }
            // Labels centred on the axis ends are kept inside or left out.
            assert_eq!(t.centred_label_x(t.left, 40.0), None);
            assert_eq!(t.centred_label_x(t.right - 10.0, 40.0), None);
            assert_eq!(t.centred_label_x(t.plot_x + t.plot_w * 0.5, 40.0), Some(t.plot_x + t.plot_w * 0.5 - 20.0));
        }
        assert!(cut_short >= 2, "the short windows have to actually cut rows for this to mean anything");
    }

    #[test]
    fn dates_thin_out_before_they_run_into_each_other() {
        let label_w = metric::AXIS_LABEL_CELLS * CELL.w;
        assert_eq!(Timeline::axis_stride(label_w * 2.0, CELL.w), 1);
        assert_eq!(Timeline::axis_stride(label_w, CELL.w), 1);
        assert_eq!(Timeline::axis_stride(label_w * 0.6, CELL.w), 2);
        assert_eq!(Timeline::axis_stride(label_w * 0.3, CELL.w), 4);
        assert_eq!(Timeline::axis_stride(0.0, CELL.w), 1);
    }

    #[test]
    fn the_shape_is_read_off_the_accounts() {
        let account = |name: &str, windows: Vec<CcUsageWindowRender>| CcUsageAccountRender {
            name: name.to_string(),
            email: String::new(),
            status_label: String::new(),
            status_severity: 0,
            reset_label: String::new(),
            windows,
        };
        let s = Shape::of(&[
            account("Claude 1", vec![window("11:59"), window("06:00")]),
            account("Codex 12", vec![window("")]),
        ]);
        assert_eq!((s.accounts, s.window_rows, s.name_cols), (2, 2, 8));
        assert_eq!(s.tag_cols, "fable 100% 11:59".len());
        let none = Shape::of(&[]);
        assert_eq!((none.accounts, none.window_rows, none.name_cols, none.tag_cols), (0, 0, 0, 0));
    }
}

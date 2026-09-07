//! cc — geometry for the Claude-usage modal.
//!
//! Same split the rest of the UI uses (`layout_modal` is the reference):
//! named metrics and the modal's own rect live here, painting lives in
//! `render_metal::paint_cc_usage_content`.  Before this module the
//! painter carried ~25 unnamed coefficients inline and the panel rect
//! was computed in a third file, so "how tall is a card?" had no single
//! answer.
//!
//! Everything is expressed as a multiple of the painter's cell metrics
//! rather than in points: the panel is drawn with the terminal cell grid
//! as its unit, so it tracks font size automatically.

/// Multipliers on the painter's cell width / height / line advance.
///
/// Grouped rather than scattered so a reader can see the rhythm of the
/// panel — outer margin, then card padding, then the row cadence — in
/// one place, and so changing "the panel feels cramped" is one edit.
pub mod metric {
    /// Line advance, as a multiple of cell height.
    pub const LINE_ADVANCE: f64 = 1.35;
    /// Panel's own margin inside its frame.
    pub const PANEL_PAD: f64 = 1.6;
    /// Gap between account cards, in cell widths.
    pub const CARD_GAP: f64 = 1.5;
    /// One padding value for all four sides of a card.
    ///
    /// Deliberately a single number in one unit: an earlier cut measured
    /// horizontal padding in cell widths and vertical padding in line
    /// advances — two different rulers — and the four gaps came out
    /// visibly unequal, with the bottom one collapsing to almost nothing
    /// once the last row's descenders were counted.
    pub const CARD_PAD: f64 = 0.85;
    /// Baseline-to-baseline advances between a card's five rows, as
    /// multiples of `LINE_ADVANCE`: name→email, email→5H, 5H→7D,
    /// 7D→reset.  Card height is derived from these, so the two can't
    /// drift apart.
    pub const CARD_ROW_ADVANCES: [f64; 4] = [1.05, 1.3, 1.3, 1.35];
    /// Breathing room between the cards section and the timeline
    /// section.  Two sections, not one continuous list.
    pub const SECTION_BREAK: f64 = 2.2;
    /// Gap under a section heading, before the content it names.
    ///
    /// A heading sitting a third of a line above its own content reads
    /// as a label stuck to the first card rather than as a heading over
    /// a group — the same reason `SECTION_BREAK` exists between the two
    /// sections.  Both headings use this, so they cannot drift apart.
    ///
    /// Sized generously (a full line plus a fifth) because the panel is
    /// a reference surface with room to spare — it already ends well
    /// short of its own frame — and because the headings are set in the
    /// larger UI font, so a gap measured in terminal lines looks
    /// tighter under them than the number suggests.
    pub const HEADING_GAP: f64 = 1.2;
    /// Horizontal padding inside a status chip, in cell widths.
    pub const CHIP_PAD: f64 = 0.6;
    /// Cap height as a fraction of ascent.  The painter reports ascent,
    /// not cap height; this is the usual stand-in and is only ever
    /// applied to the fixed all-caps status labels, where centring on
    /// the ink (not the baseline box) is what makes the chip look right.
    pub const CAP_HEIGHT_OF_ASCENT: f64 = 0.72;
    /// Utilisation bar thickness, as a multiple of cell height.
    pub const BAR_H: f64 = 0.62;
    /// Vertical gap between a timeline row's two window bars.
    pub const TIMELINE_BAR_GAP: f64 = 0.78;
    /// Extra height per timeline row beyond its two bars and their gap.
    pub const TIMELINE_ROW_EXTRA: f64 = 1.0;
    /// Day-rule dash and gap lengths.
    pub const GRID_DASH: f64 = 0.30;
    pub const GRID_GAP: f64 = 0.26;
}

/// Fallback timeline span when there is nothing to measure: ±6 days
/// around now, in seconds.  Real spans come from [`timeline_range`].
pub const TIMELINE_SPAN_SECS: f64 = 12.0 * 86_400.0;


/// The instants the plot must cover: every bar's full extent, plus now.
///
/// This used to be a fixed ±6 days, which does not fit the data it is
/// drawing: a 7-day window resets up to 7 days out, so its bar ran off
/// the right end and was clamped there — ending at the axis edge
/// instead of at the time written on its own label.  Measured on the
/// live feed: two of four 7d bars ended at 1.014 and 1.056 of the
/// span, i.e. both were cut short, and the further out the reset the
/// more the label lied about where the bar stopped.
///
/// Deriving the range from the extents makes clamping impossible, which
/// is what lets the label sit immediately right of a bar end that is
/// always the truth.
pub fn timeline_range(now: i64, extents: &[(i64, i64)]) -> (f64, f64) {
    let mut lo = now as f64;
    let mut hi = now as f64;
    for &(start, end) in extents {
        lo = lo.min(start as f64);
        hi = hi.max(end as f64);
    }
    if hi - lo < 3_600.0 {
        // Degenerate feed (everything resets within the hour): fall
        // back to the fixed span rather than draw a plot whose scale
        // magnifies clock skew.
        let n = now as f64;
        return (n - TIMELINE_SPAN_SECS / 2.0, n + TIMELINE_SPAN_SECS / 2.0);
    }
    // No percentage padding: the caller snaps this outward to local
    // day boundaries, and that IS the margin — one mechanism instead
    // of two.  A percentage pad on top only ever pushed the snap a
    // whole further day out, wasting plot width.
    (lo, hi)
}

/// Height of one account card in physical px.
///
/// Derived, never a round number: padding, the first row's ascent, the
/// row advances, the last row's descent, padding again.  `cell_h -
/// ascent` is the descent — a monospace cell is exactly the two
/// stacked.  Writing this as a literal is how the bottom padding got
/// lost the first time.
/// `window_rows` is how many bar rows the tallest card draws — every
/// window the account is metered on, not just the extras.  One number
/// instead of "two, plus however many more": the providers disagree
/// about what the first two are, and a caller that has to subtract
/// before calling is a caller that will one day subtract wrong.
pub fn card_height(cell_h: f64, ascent: f64, window_rows: usize) -> f64 {
    let lh = cell_h * metric::LINE_ADVANCE;
    let pad = cell_h * metric::CARD_PAD;
    let rows: f64 = metric::CARD_ROW_ADVANCES.iter().map(|m| lh * m).sum();
    // The four advances above already cover two bar rows; each further
    // one steps by the same amount the second→third step uses.  Derived
    // rather than a second literal: a card whose height and whose
    // painter disagree is the bug this module exists to prevent.
    let extra = lh * metric::CARD_ROW_ADVANCES[2] * window_rows.saturating_sub(2) as f64;
    pad * 2.0 + ascent + rows + extra + (cell_h - ascent)
}

/// Fixed chrome height of the whole panel, in line advances: the title
/// heading and its gap, one account card, the section break, the
/// timeline heading and its gap, and the panel's top and bottom
/// margins.  The caller adds one band per timeline row plus the date
/// axis.
pub const PANEL_CHROME_LINES: f64 = 16.8;
/// One timeline band, in line advances, for an account drawing `bars`
/// windows.  The painter derives its own row height from the same
/// metrics; this is that height expressed in the unit `panel_rect`
/// budgets in, so the panel cannot come up short of what it draws.
pub fn timeline_row_lines(bars: usize) -> f64 {
    let n = bars.max(1) as f64;
    let bars_h = metric::BAR_H * n + metric::TIMELINE_BAR_GAP * (n - 1.0);
    bars_h / metric::LINE_ADVANCE + metric::TIMELINE_ROW_EXTRA
}
/// Date axis plus bottom margin, in line advances.
pub const AXIS_LINES: f64 = 4.0;

/// Panel rect for `n` accounts, centred in a `w_phys × h_phys` window
/// below `top_inset`.
///
/// Width tracks account count so four cards stay readable, capped at
/// 94 % of the window; height is the chrome plus one band per account.
pub fn panel_rect(
    n_accounts: usize,
    window_rows: usize,
    w_phys: f64,
    h_phys: f64,
    cell_w: f64,
    cell_h: f64,
    top_inset: f64,
) -> marspot_term::layout::Rect {
    let n = n_accounts.max(1) as f64;
    let lh = cell_h * metric::LINE_ADVANCE;
    let w = (w_phys * 0.94)
        .min(n * 52.0 * cell_w + 8.0 * cell_w)
        .max(64.0 * cell_w)
        .min(w_phys - 24.0);
    // The chrome figure covers a card with two bar rows; every further
    // window makes each card taller by the same step the painter uses,
    // and makes each timeline band taller by one more bar.
    let cards_extra = lh * metric::CARD_ROW_ADVANCES[2] * window_rows.saturating_sub(2) as f64;
    let h = (lh * PANEL_CHROME_LINES
        + cards_extra
        + n * lh * timeline_row_lines(window_rows)
        + lh * AXIS_LINES)
        .min(h_phys * 0.9);
    marspot_term::layout::Rect {
        x: (w_phys - w) / 2.0,
        y_top: ((h_phys - h) / 2.0).max(top_inset + 8.0),
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bar must fit: the plot exists to show where each window
    /// ends, and a bar clamped at the axis edge says the opposite of
    /// what its own label says.
    ///
    /// Numbers are the live feed of 2026-08-01 (four accounts, both
    /// windows each).  Under the old fixed ±6 d span, Claude 1's and
    /// Claude 4's 7-day bars ended at 1.014 and 1.056 of the span.
    #[test]
    fn every_window_fits_inside_the_range() {
        let now = 1785538544i64;
        let resets_7d = [1786071600i64, 1785704400, 1785646800, 1786114800];
        let resets_5h = [1785543600i64, 1785544800, 1785543600, 1785546000];
        let extents: Vec<(i64, i64)> = resets_5h
            .iter()
            .map(|r| (r - 5 * 3_600, *r))
            .chain(resets_7d.iter().map(|r| (r - 7 * 86_400, *r)))
            .collect();
        let (t0, t1) = timeline_range(now, &extents);
        for (start, end) in extents {
            let f0 = (start as f64 - t0) / (t1 - t0);
            let f1 = (end as f64 - t0) / (t1 - t0);
            assert!(f0 >= 0.0 && f1 <= 1.0, "window {start}..{end} escapes the plot: {f0}..{f1}");
        }
        // Now is inside it too — the marker has to land somewhere real.
        assert!((now as f64) > t0 && (now as f64) < t1);
    }

    /// A feed whose windows all reset within the hour would otherwise
    /// be drawn at a scale where clock skew looks like a day.
    #[test]
    fn a_degenerate_feed_falls_back_to_the_fixed_span() {
        let now = 1785538544i64;
        let (t0, t1) = timeline_range(now, &[(now - 60, now + 60)]);
        assert!((t1 - t0 - TIMELINE_SPAN_SECS).abs() < 1.0);
    }

    /// Card height must account for the last row's descenders — the
    /// bug this derivation replaced was a hardcoded multiple that left
    /// the bottom row sitting on (or past) the card border.
    #[test]
    fn card_height_leaves_room_below_the_last_baseline() {
        let (cell_h, ascent) = (20.0, 15.0);
        let lh = cell_h * metric::LINE_ADVANCE;
        let pad = cell_h * metric::CARD_PAD;
        let rows: f64 = metric::CARD_ROW_ADVANCES.iter().map(|m| lh * m).sum();

        // Two windows is the smallest card; the rows beyond them are
        // exactly the reason a card can outgrow its own box, and a
        // Codex account draws four.
        for window_rows in [2usize, 3, 4, 5] {
            let h = card_height(cell_h, ascent, window_rows);
            // Where the last row's baseline lands, measured from the top.
            let last_baseline = pad
                + ascent
                + rows
                + lh * metric::CARD_ROW_ADVANCES[2] * window_rows.saturating_sub(2) as f64;
            let below = h - last_baseline;
            let descent = cell_h - ascent;
            assert!(
                below >= descent + pad - 0.001,
                "window_rows={window_rows}: only {below} px below the last baseline; \
                 needs descent ({descent}) + padding ({pad})"
            );
        }
    }

    /// The panel budgets one band per account in line advances while
    /// the painter measures the same band in cell heights.  Two units,
    /// one number — so they are checked against each other here rather
    /// than trusted to stay in step.
    #[test]
    fn a_timeline_band_is_budgeted_for_every_bar_it_draws() {
        let cell_h = 20.0;
        let lh = cell_h * metric::LINE_ADVANCE;
        for bars in 1..=6usize {
            let n = bars as f64;
            let painter = cell_h * metric::BAR_H * n
                + cell_h * metric::TIMELINE_BAR_GAP * (n - 1.0)
                + lh * metric::TIMELINE_ROW_EXTRA;
            let budget = lh * timeline_row_lines(bars);
            assert!(
                budget >= painter - 0.001,
                "bars={bars}: budgeted {budget} px for a band the painter draws at {painter}"
            );
        }
    }

    /// Padding is one value in one unit, so top and bottom match.
    #[test]
    fn card_padding_is_symmetric() {
        let (cell_h, ascent) = (20.0, 15.0);
        let pad = cell_h * metric::CARD_PAD;
        let h = card_height(cell_h, ascent, 2);
        let lh = cell_h * metric::LINE_ADVANCE;
        let rows: f64 = metric::CARD_ROW_ADVANCES.iter().map(|m| lh * m).sum();
        let top_gap = pad;
        let bottom_gap = h - (pad + ascent + rows + (cell_h - ascent));
        assert!(
            (top_gap - bottom_gap).abs() < 0.001,
            "top {top_gap} vs bottom {bottom_gap}"
        );
    }

    /// The panel never overflows the window it is centred in.
    #[test]
    fn panel_rect_stays_inside_the_window() {
        for n in 1..=8 {
            let r = panel_rect(n, 3, 1600.0, 1000.0, 8.0, 18.0, 60.0);
            assert!(r.x >= 0.0, "n={n} x={}", r.x);
            assert!(r.x + r.w <= 1600.0 + 0.001, "n={n} overflows width");
            assert!(r.y_top >= 60.0, "n={n} must clear the top inset");
            assert!(r.h <= 1000.0 * 0.9 + 0.001, "n={n} too tall");
        }
    }
}

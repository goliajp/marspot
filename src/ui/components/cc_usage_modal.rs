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

/// Breathing room at each end of the plot, as a fraction of the span,
/// so the outermost bar's rounded cap isn't flush against the axis.
const RANGE_PAD: f64 = 0.02;

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
    let pad = (hi - lo) * RANGE_PAD;
    (lo - pad, hi + pad)
}

/// Height of one account card in physical px.
///
/// Derived, never a round number: padding, the first row's ascent, the
/// row advances, the last row's descent, padding again.  `cell_h -
/// ascent` is the descent — a monospace cell is exactly the two
/// stacked.  Writing this as a literal is how the bottom padding got
/// lost the first time.
pub fn card_height(cell_h: f64, ascent: f64, extra_bar_rows: usize) -> f64 {
    let lh = cell_h * metric::LINE_ADVANCE;
    let pad = cell_h * metric::CARD_PAD;
    let rows: f64 = metric::CARD_ROW_ADVANCES.iter().map(|m| lh * m).sum();
    // Each per-model cap adds one more bar row, on the same advance the
    // 5H→7D step uses.  Derived rather than a second literal: a card
    // whose height and whose painter disagree is the bug this module
    // exists to prevent.
    let extra = lh * metric::CARD_ROW_ADVANCES[2] * extra_bar_rows as f64;
    pad * 2.0 + ascent + rows + extra + (cell_h - ascent)
}

/// Fixed chrome height of the whole panel, in line advances: the title
/// heading, one account card, the section break, the timeline heading,
/// and the panel's top and bottom margins.  The caller adds one band
/// per timeline row plus the date axis.
pub const PANEL_CHROME_LINES: f64 = 15.0;
/// One timeline row's band, in line advances.
pub const TIMELINE_ROW_LINES: f64 = 2.6;
/// Date axis plus bottom margin, in line advances.
pub const AXIS_LINES: f64 = 4.0;

/// Panel rect for `n` accounts, centred in a `w_phys × h_phys` window
/// below `top_inset`.
///
/// Width tracks account count so four cards stay readable, capped at
/// 94 % of the window; height is the chrome plus one band per account.
pub fn panel_rect(
    n_accounts: usize,
    extra_bar_rows: usize,
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
    // The chrome figure covers a card with the two account windows;
    // per-model rows make every card taller by the same step the
    // painter uses.
    let cards_extra = lh * metric::CARD_ROW_ADVANCES[2] * extra_bar_rows as f64;
    let h = (lh * PANEL_CHROME_LINES + cards_extra + n * lh * TIMELINE_ROW_LINES + lh * AXIS_LINES)
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

        // Checked with and without per-model rows: the extra rows are
        // exactly the reason a card can outgrow its own box.
        for extra in [0usize, 1, 3] {
            let h = card_height(cell_h, ascent, extra);
            // Where the last row's baseline lands, measured from the top.
            let last_baseline =
                pad + ascent + rows + lh * metric::CARD_ROW_ADVANCES[2] * extra as f64;
            let below = h - last_baseline;
            let descent = cell_h - ascent;
            assert!(
                below >= descent + pad - 0.001,
                "extra={extra}: only {below} px below the last baseline; \
                 needs descent ({descent}) + padding ({pad})"
            );
        }
    }

    /// Padding is one value in one unit, so top and bottom match.
    #[test]
    fn card_padding_is_symmetric() {
        let (cell_h, ascent) = (20.0, 15.0);
        let pad = cell_h * metric::CARD_PAD;
        let h = card_height(cell_h, ascent, 0);
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
            let r = panel_rect(n, 1, 1600.0, 1000.0, 8.0, 18.0, 60.0);
            assert!(r.x >= 0.0, "n={n} x={}", r.x);
            assert!(r.x + r.w <= 1600.0 + 0.001, "n={n} overflows width");
            assert!(r.y_top >= 60.0, "n={n} must clear the top inset");
            assert!(r.h <= 1000.0 * 0.9 + 0.001, "n={n} too tall");
        }
    }
}

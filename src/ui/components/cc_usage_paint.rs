//! The agent-accounts panel: how its cards and its timeline are
//! painted.
//!
//! The frame, the header and the group headings are the panel's tree
//! (`cc_usage_view`); every rect drawn here comes from
//! `cc_usage_layout`, which is also what sizes the panel.  Prose (names, addresses, the reset line) is set in the
//! panel type roles; the bars' labels, percentages and the whole
//! timeline are tabular data and stay in the terminal's font.

use marspot_term::layout::Rect;

use crate::ui::components::cc_usage_layout::{card_rows, Cell, Timeline};
use crate::ui::components::cc_usage_modal::{
    fit_ellipsis, fit_measured, metric, timeline_range, CcUsageAccountRender, CcUsageRender,
};
use crate::ui::components::controls;
use crate::ui::components::panel_chrome;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::view::ViewPainter;
use crate::ui::strings::{self, Msg};
use crate::ui::theme::{spec, PanelText};

/// `s` in `role`, cut to `max_w` with an ellipsis.
fn fit_role(p: &mut ViewPainter, role: PanelText, s: &str, max_w: f64) -> String {
    fit_measured(s, max_w, &mut |t| p.panel_text_width(role, t) as f64)
}

/// A line of the terminal font, vertically centred in the box
/// `top .. top + h`.
fn mono(p: &mut ViewPainter, cell: Cell, x: f64, top: f64, h: f64, s: &str, color: [f32; 4]) {
    p.text(x as f32, cell.baseline_in(top, h) as f32, s, color);
}

/// How a utilisation is doing: fine, approaching the cap, at it.
fn util_severity(util: f32) -> u8 {
    if util < metric::WARN_AT {
        0
    } else if util < metric::DANGER_AT {
        1
    } else {
        2
    }
}

fn fill(p: &mut ViewPainter, rect: Rect, color: [f32; 4]) {
    p.fill_rounded_rect(rect, color, spec::px(metric::BAR_RADIUS) as f32, (palette::none(), 0.0));
}

/// A usage bar: the track over the whole of `rect`, the colour over
/// the used part of it.  No reading leaves the track alone.
fn paint_bar(p: &mut ViewPainter, rect: Rect, util: Option<f32>) {
    if rect.w < metric::BAR_MIN_PX {
        return;
    }
    fill(p, rect, palette::track());
    let Some(util) = util else { return };
    let w = rect.w * (util as f64).clamp(0.0, 1.0);
    if w >= metric::BAR_MIN_PX {
        fill(p, Rect { w, ..rect }, palette::severity_color(util_severity(util)));
    }
}

/// The sentence shown before either feed has been read, in the middle
/// of the content area.
pub(crate) fn paint_empty(p: &mut ViewPainter, content: Rect) {
    let text = fit_role(p, metric::EMPTY, strings::t(metric::EMPTY_TEXT), content.w);
    let w = p.panel_text_width(metric::EMPTY, &text) as f64;
    let baseline = p.ui_baseline_centred(content.y_top as f32, content.h as f32, metric::EMPTY.pt());
    p.panel_text(
        metric::EMPTY, (content.x + (content.w - w) * 0.5) as f32, baseline, &text, palette::fg_sec(),
    );
}

/// One account: name and status, address, a bar per metered window,
/// when the next window renews.
pub(crate) fn paint_card(p: &mut ViewPainter, a: &CcUsageAccountRender, card: Rect, cell: Cell, window_rows: usize) {
    panel_chrome::card(p, card);
    let rows = card_rows(card, cell.h, window_rows);

    // The chip takes what it needs first; the name gets the rest of
    // the line, so no pairing of lengths can make them collide.
    let mut name_w = rows.right - rows.left;
    let status = a.status_label.to_ascii_uppercase();
    if !status.is_empty() {
        let size = controls::chip_size(&status, &mut controls::font_measure(&mut *p.font));
        let chip = rows.chip(size);
        controls::paint_chip(p, chip, &status, a.status_severity);
        name_w = chip.x - spec::px(metric::NAME_TO_CHIP) - rows.left;
    }
    let name = fit_role(p, metric::NAME, &a.name, name_w);
    let baseline = p.ui_baseline_centred(
        rows.name_line.y_top as f32, rows.name_line.h as f32, metric::NAME.pt(),
    );
    p.panel_text(metric::NAME, rows.left as f32, baseline, &name, palette::fg());

    let email = fit_role(p, metric::EMAIL, &a.email, rows.right - rows.left);
    p.panel_text(metric::EMAIL, rows.left as f32, rows.email_baseline as f32, &email, palette::fg_sec());

    // Labels are not all the same length, so the bars start after the
    // widest one rather than at a fixed column.
    let label_cols = a.windows.iter().map(|w| w.label.chars().count()).max().unwrap_or(0);
    for (i, w) in a.windows.iter().enumerate() {
        let row = rows.bar_row(i, label_cols, cell);
        mono(p, cell, rows.left, row.top, row.h, &w.label, palette::fg_muted());
        let pct = w.percent();
        let color = w.util.map_or(palette::fg_muted(), |u| palette::severity_text(util_severity(u)));
        mono(p, cell, row.pct_right - cell.text_w(&pct), row.top, row.h, &pct, color);
        paint_bar(p, row.bar, w.util);
    }

    let reset = fit_role(p, metric::RESET, &a.reset_label, rows.right - rows.left);
    p.panel_text(metric::RESET, rows.left as f32, rows.reset_baseline as f32, &reset, palette::fg_muted());
}

/// The timeline: one row per account, each of its windows a bar from
/// when the window opened to when it renews, on one time axis.
pub(crate) fn paint_timeline(p: &mut ViewPainter, cc: &CcUsageRender, t: &Timeline, cell: Cell) {
    panel_chrome::card(p, t.card);
    // Scale to what has to be drawn, then out to whole days: that is
    // the plot's margin, and it puts a dated rule on both sides of
    // every bar end.
    let extents: Vec<(i64, i64)> = cc
        .accounts
        .iter()
        .flat_map(|a| a.windows.iter())
        .filter_map(|w| w.reset_unix.map(|r| (r - w.span_secs, r)))
        .collect();
    let (t0, t1) = timeline_range(cc.clock.now_unix, &extents);
    let (t0, t1) = cc.clock.snap_to_days(t0, t1);
    let frac = |time: f64| (time - t0) / (t1 - t0);

    paint_days(p, cc, t, cell, t0, t1);

    for (row, a) in cc.accounts.iter().take(t.rows).enumerate() {
        let name = fit_ellipsis(&a.name, t.name_cols);
        mono(p, cell, t.left, t.row_top(row), t.row_h, &name, palette::fg());
        for (idx, w) in a.windows.iter().enumerate() {
            // No reset means no extent: the card still names the
            // window, but there is nothing here to draw it against.
            let Some(reset) = w.reset_unix else { continue };
            let (start, end) = ((reset - w.span_secs) as f64, reset as f64);
            // The track is the window's whole extent; the colour
            // covers the used part of it.
            let bar = t.bar(row, idx, a.windows.len(), cell.h, frac(start), frac(end));
            paint_bar(p, bar, w.util);
            let (x, tag) = t.tag(&w.tag(), bar.x + bar.w, cell.w);
            mono(p, cell, x, bar.y_top, bar.h, &tag, palette::fg_sec());
        }
    }

    // The present: a solid line through every row, named above them.
    let now_x = t.x_of(frac(cc.clock.now_unix as f64));
    let line_top = t.now_top + cell.h;
    p.fill_rounded_rect(
        Rect { x: now_x, y_top: line_top, w: spec::px(metric::NOW_W), h: t.rows_bottom() - line_top },
        palette::now(), 0.0, (palette::none(), 0.0),
    );
    let label = strings::t(Msg::TimelineNow);
    let w = cell.text_w(label);
    let x = (now_x - w * 0.5).clamp(t.left, (t.right - w).max(t.left));
    mono(p, cell, x, t.now_top, cell.h, label, palette::now());
}

/// A dashed rule up through the rows at every midnight, and its date
/// under it.
///
/// The rules carry each date up into the plot, so a bar's end is read
/// against a date in place.  Dashed and faint because they are
/// reference: the one line that has to stay solid is the present.
fn paint_days(p: &mut ViewPainter, cc: &CcUsageRender, t: &Timeline, cell: Cell, t0: f64, t1: f64) {
    let dash = cell.h * metric::GRID_DASH_CELLS;
    let gap = cell.h * metric::GRID_GAP_CELLS;
    let bottom = t.rows_bottom();
    let day_w = t.plot_w * 86_400.0 / (t1 - t0);
    let stride = Timeline::axis_stride(day_w, cell.w);
    // The range starts and ends on midnights, so the first rule is its
    // start and the last its end.
    let mut day = t0 as i64;
    let mut nth = 0usize;
    while day as f64 <= t1 {
        let x = t.x_of((day as f64 - t0) / (t1 - t0));
        let mut y = t.rows_top;
        while y < bottom {
            p.fill_rounded_rect(
                Rect { x, y_top: y, w: metric::DAY_RULE_PX, h: dash.min(bottom - y) },
                palette::grid(), 0.0, (palette::none(), 0.0),
            );
            y += dash + gap;
        }
        if nth.is_multiple_of(stride) {
            let (month, d) = cc.clock.month_day(day);
            let label = format!("{month}/{d}");
            // A date that would cross the card's padding is left out.
            if let Some(lx) = t.centred_label_x(x, cell.text_w(&label)) {
                mono(p, cell, lx, t.axis_top, cell.h, &label, palette::fg_muted());
            }
        }
        day = cc.clock.next_day_start(day);
        nth += 1;
    }
}

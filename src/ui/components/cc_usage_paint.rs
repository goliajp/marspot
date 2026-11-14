//! The agent-accounts panel: how its timeline is painted.
//!
//! Everything else in the panel is its tree (`cc_usage_view`).  The
//! timeline is a plot, drawn here as one node of that tree on the
//! rects `cc_usage_layout` gives it, in the terminal's font.

use std::fmt::Write;

use marspot_term::layout::Rect;

use crate::ui::components::cc_usage_layout::{Cell, Timeline};
use crate::ui::components::cc_usage_modal::{cut_to_columns, metric, timeline_range, CcUsageRender};
use crate::ui::components::panel_chrome;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::view::ViewPainter;
use crate::ui::strings::{self, Msg};
use crate::ui::theme::spec;

/// A line of the terminal font, vertically centred in the box
/// `top .. top + h`.
fn mono(p: &mut ViewPainter, cell: Cell, x: f64, top: f64, h: f64, s: &str, color: [f32; 4]) {
    p.text(x as f32, cell.baseline_in(top, h) as f32, s, color);
}

/// How a utilisation is doing: fine, approaching the cap, at it.
pub(crate) fn util_severity(util: f32) -> u8 {
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

/// The timeline: one row per account, each of its windows a bar from
/// when the window opened to when it renews, on one time axis.
/// `scratch` is what each label is composed in.
pub(crate) fn paint_timeline(p: &mut ViewPainter, cc: &CcUsageRender, t: &Timeline, cell: Cell, scratch: &mut String) {
    panel_chrome::card(p, t.card);
    // Scale to what has to be drawn, then out to whole days: that is
    // the plot's margin, and it puts a dated rule on both sides of
    // every bar end.
    let extents = cc
        .accounts
        .iter()
        .flat_map(|a| a.windows.iter())
        .filter_map(|w| w.reset_unix.map(|r| (r - w.span_secs, r)));
    let (t0, t1) = timeline_range(cc.clock.now_unix, extents);
    let (t0, t1) = cc.clock.snap_to_days(t0, t1);
    let frac = |time: f64| (time - t0) / (t1 - t0);

    paint_days(p, cc, t, cell, (t0, t1), scratch);

    for (row, a) in cc.accounts.iter().take(t.rows).enumerate() {
        scratch.clear();
        scratch.push_str(&a.name);
        cut_to_columns(scratch, t.name_cols);
        mono(p, cell, t.left, t.row_top(row), t.row_h, scratch, palette::fg());
        for (idx, w) in a.windows.iter().enumerate() {
            // No reset means no extent: the card still names the
            // window, but there is nothing here to draw it against.
            let Some(reset) = w.reset_unix else { continue };
            let (start, end) = ((reset - w.span_secs) as f64, reset as f64);
            // The track is the window's whole extent; the colour
            // covers the used part of it.
            let bar = t.bar(row, idx, a.windows.len(), cell.h, frac(start), frac(end));
            paint_bar(p, bar, w.util);
            scratch.clear();
            w.write_tag(scratch);
            let x = t.cut_tag(scratch, bar.x + bar.w, cell.w);
            mono(p, cell, x, bar.y_top, bar.h, scratch, palette::fg_sec());
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
fn paint_days(p: &mut ViewPainter, cc: &CcUsageRender, t: &Timeline, cell: Cell, (t0, t1): (f64, f64), scratch: &mut String) {
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
            scratch.clear();
            let _ = write!(scratch, "{month}/{d}");
            // A date that would cross the card's padding is left out.
            if let Some(lx) = t.centred_label_x(x, cell.text_w(scratch)) {
                mono(p, cell, lx, t.axis_top, cell.h, scratch, palette::fg_muted());
            }
        }
        day = cc.clock.next_day_start(day);
        nth += 1;
    }
}

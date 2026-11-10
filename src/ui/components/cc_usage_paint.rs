//! The agent-accounts panel: how a frame of it is painted.
//!
//! The frame and header are the shared panel chrome's; every rect
//! drawn here comes from `cc_usage_layout`, which is also what sizes
//! the panel.  Prose (names, addresses, the reset line) is set in the
//! panel type roles; the bars' labels, percentages and the whole
//! timeline are tabular data and stay in the terminal's font.

use marspot_term::layout::Rect;

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::ui::components::cc_usage_layout::{card_rows, layout, Cell, Shape, Timeline};
use crate::ui::components::cc_usage_modal::{
    fit_ellipsis, fit_measured, metric, timeline_range, CcUsageAccountRender, CcUsageRender, Hover,
};
use crate::ui::components::controls;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::components::panel_palette as palette;
use crate::ui::core::view::ViewPainter;
use crate::ui::theme::{spec, PanelText};
use golia_ui_core::scene::{GlyphInstance, RectInstance as CellInstance, UiRectInstance};

/// The agent-accounts panel: a card per account, then the timeline of
/// when each account's windows renew.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_cc_usage_via_view(
    cc: &CcUsageRender,
    top_inset: f64,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    window_w: f64,
    window_h: f64,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
) {
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let cell = Cell { w: cell_w as f64, h: cell_h as f64, ascent: ascent as f64 };
    let chrome = PanelChrome::new(cc.rect, top_inset);
    let meta = (!cc.updated_label.is_empty()).then_some(cc.updated_label.as_str());
    chrome.paint(&mut painter, metric::TITLE, meta, cc.hover == Hover::Close, |p, content| {
        if cc.accounts.is_empty() {
            paint_empty(p, content);
            return;
        }
        let shape = Shape::of(&cc.accounts);
        let l = layout(content, cell, &shape);
        let count = cc.accounts.len().to_string();
        panel_chrome::section_heading(
            p, content.x, l.heading_top, content.w, metric::ACCOUNTS_HEADING, Some(&count),
        );
        for (account, card) in cc.accounts.iter().zip(&l.cards) {
            paint_card(p, account, *card, cell, shape.window_rows);
        }
        if let Some(t) = &l.timeline {
            panel_chrome::section_heading(
                p, content.x, t.heading_top, content.w, metric::TIMELINE_HEADING, None,
            );
            paint_timeline(p, cc, t, cell);
        }
    });
}

/// `s` in `role`, cut to `max_w` with an ellipsis.
fn fit_role(p: &mut ViewPainter, role: PanelText, s: &str, max_w: f64) -> String {
    fit_measured(s, max_w, &mut |t| p.panel_text_width(role, t) as f64)
}

/// A line of the terminal font, vertically centred in the box
/// `top .. top + h`.
fn mono(p: &mut ViewPainter, cell: Cell, x: f64, top: f64, h: f64, s: &str, color: [f32; 4]) {
    p.text(x as f32, cell.baseline_in(top, h) as f32, s, color);
}

fn util_color(util: f32) -> [f32; 4] {
    if util < metric::WARN_AT {
        palette::ok()
    } else if util < metric::DANGER_AT {
        palette::warn()
    } else {
        palette::danger()
    }
}

fn fill(p: &mut ViewPainter, rect: Rect, color: [f32; 4]) {
    p.fill_rounded_rect(rect, color, spec::px(metric::BAR_RADIUS) as f32, (palette::none(), 0.0));
}

/// A usage bar: the track over the whole of `rect`, the colour over
/// the first `used` of it.
fn paint_bar(p: &mut ViewPainter, rect: Rect, used: f64, util: f32) {
    if rect.w < metric::BAR_MIN_PX {
        return;
    }
    fill(p, rect, palette::track());
    let w = rect.w * used.clamp(0.0, 1.0);
    if w >= metric::BAR_MIN_PX {
        fill(p, Rect { w, ..rect }, util_color(util));
    }
}

/// The sentence shown before either feed has been read, in the middle
/// of the content area.
fn paint_empty(p: &mut ViewPainter, content: Rect) {
    let text = fit_role(p, metric::EMPTY, metric::EMPTY_TEXT, content.w);
    let w = p.panel_text_width(metric::EMPTY, &text) as f64;
    let baseline = p.ui_baseline_centred(content.y_top as f32, content.h as f32, metric::EMPTY.pt());
    p.panel_text(
        metric::EMPTY, (content.x + (content.w - w) * 0.5) as f32, baseline, &text, palette::fg_sec(),
    );
}

/// One account: name and status, address, a bar per metered window,
/// when the next window renews.
fn paint_card(p: &mut ViewPainter, a: &CcUsageAccountRender, card: Rect, cell: Cell, window_rows: usize) {
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
        let pct = format!("{:.0}%", w.util * 100.0);
        mono(p, cell, row.pct_right - cell.text_w(&pct), row.top, row.h, &pct, util_color(w.util));
        paint_bar(p, row.bar, w.util as f64, w.util);
    }

    let reset = fit_role(p, metric::RESET, &a.reset_label, rows.right - rows.left);
    p.panel_text(metric::RESET, rows.left as f32, rows.reset_baseline as f32, &reset, palette::fg_muted());
}

/// The timeline: one row per account, each of its windows a bar from
/// when the window opened to when it renews, on one time axis.
fn paint_timeline(p: &mut ViewPainter, cc: &CcUsageRender, t: &Timeline, cell: Cell) {
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
            paint_bar(p, bar, w.util as f64, w.util);
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
    let label = "NOW";
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

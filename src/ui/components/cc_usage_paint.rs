//! The agent-usage modal: how a frame of it is painted.
//!
//! Its data and geometry are in `cc_usage_modal`; this is the painter,
//! drawn through a `ViewPainter` into the overlay scratches.

use marspot_term::layout::Rect;

use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::render_metal::{CellInstance, GlyphInstance, UiRectInstance};
use crate::ui::components::cc_usage_modal::{
    card_height, cc_bars_height, fit_ellipsis, metric, timeline_range, timeline_row_height,
    CcUsageRender, CcUsageWindowRender,
};
use crate::ui::components::modal_frame::{MODAL_FRAME_BG, MODAL_FRAME_BORDER, MODAL_FRAME_CORNER_RADIUS};
use crate::ui::components::panel_palette;

/// cc — paint the `Cc` usage modal: one card per Claude account
/// (5H / 7D utilization bars + reset instants) and a ±6-day
/// availability timeline underneath (per-account 5h/7d window bars,
/// NOW marker, daily ticks).  All geometry derives from cell
/// metrics so the modal scales with the font.
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
    use crate::ui::core::view::{View, ViewStyle, ViewPainter, Backdrop};
    let mut painter = ViewPainter {
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        window_w, window_h,
        font, atlas, cells, glyphs, ui_rects,
    };
    let view = View {
        rect: cc.rect,
        style: ViewStyle {
            bg: MODAL_FRAME_BG,
            border_color: MODAL_FRAME_BORDER,
            border_width: 1.0,
            corner_radius: MODAL_FRAME_CORNER_RADIUS,
            shadow_blur: 16.0,
            shadow_alpha: 0.45,
            padding: 0.0,
            backdrop: Backdrop::Dim {
                color: [0.0, 0.0, 0.0, 0.45],
                exclude_above_y: top_inset,
            },
        },
    };
    view.paint(&mut painter, |p| {
        paint_cc_usage_content(cc, p);
    });
}


fn cc_util_color(util: f32) -> [f32; 4] {
    if util < 0.5 {
        panel_palette::ok()
    } else if util < 0.85 {
        panel_palette::warn()
    } else {
        panel_palette::danger()
    }
}

/// A timeline bar's tag: `7d 100% 12:00`.
fn cc_window_tag(w: &CcUsageWindowRender) -> String {
    format!("{} {:.0}% {}", w.label, w.util * 100.0, w.reset_hm)
}



fn paint_cc_usage_content(cc: &CcUsageRender, p: &mut crate::ui::core::view::ViewPainter) {
    let cw = p.cell_w;
    let ch = p.cell_h;
    let ascent = p.ascent;
    let r = cc.rect;
    // Outer breathing room.  The panel is a reference surface, not a
    // dense readout — it can afford to sit away from its own frame.
    let pad = ch as f64 * metric::PANEL_PAD;
    let lh = ch as f64 * metric::LINE_ADVANCE;
    let inner_x = r.x + pad;
    let inner_w = (r.w - 2.0 * pad).max(1.0);
    let g = UsageGeometry { cw, ch, ascent, r, pad, lh, inner_x, inner_w };
    let Some(y) = paint_usage_title(cc, p, &g) else { return };
    let y = paint_usage_cards(cc, p, &g, y);
    paint_usage_timeline(cc, p, &g, y);
}

/// The measures every section of the panel is laid out from.
#[derive(Clone, Copy)]
struct UsageGeometry {
    cw: f32,
    ch: f32,
    ascent: f32,
    r: Rect,
    pad: f64,
    lh: f64,
    inner_x: f64,
    inner_w: f64,
}

/// The title row. `None` when there is no feed: the panel says so and
/// stops there.
fn paint_usage_title(
    cc: &CcUsageRender,
    p: &mut crate::ui::core::view::ViewPainter,
    g: &UsageGeometry,
) -> Option<f64> {
    let UsageGeometry { cw, ch, ascent, r, pad, lh, inner_x, inner_w, .. } = *g;
    let text = |p: &mut crate::ui::core::view::ViewPainter, x: f64, baseline: f64, s: &str, c: [f32; 4]| {
        p.text(x as f32, baseline as f32, s, c);
    };
    let text_w = |s: &str| -> f64 { s.chars().count() as f64 * cw as f64 };

    // ---- title row ----
    // The heading renders in the system UI font at the system size, so
    // it sits at parity with the native macOS window header rather than
    // in the smaller terminal mono face.  The right-aligned "updated"
    // stamp stays mono — it is a timestamp, tabular by nature — and is
    // vertically centred against the taller heading line.
    let head_top = r.y_top + pad;
    let ui_line = p.ui_line_h() as f64;
    // "AGENT", not "CLAUDE": the panel carries every provider whose
    // feed is on disk, and naming it after one of them is how a reader
    // concludes the other is missing rather than absent.
    let title = format!("AGENT ACCOUNTS  {}", cc.accounts.len());
    // Same role, same code path as every other panel's title — the
    // point of the ladder is that "title" means one size app-wide.
    let title_role = crate::ui::theme::PanelText::Title;
    p.panel_text(
        title_role,
        inner_x as f32,
        (head_top + p.ui_ascent() as f64) as f32,
        &title,
        panel_palette::fg(),
    );
    let upd = &cc.updated_label;
    let upd_baseline = head_top + (ui_line - ch as f64) * 0.5 + ascent as f64;
    text(p, inner_x + inner_w - text_w(upd), upd_baseline, upd, panel_palette::fg_sec());
    let mut y = head_top + ui_line + lh * metric::HEADING_GAP;

    if cc.feed_missing {
        y += lh;
        text(p, inner_x, y, "no usage feed at ~/.local/state/devops/{claude,codex}-usage.json", panel_palette::fg_sec());
        return None;
    }
    Some(y)
}

/// The account cards, one row of them, starting at `y`. Returns where
/// the next section starts.
fn paint_usage_cards(
    cc: &CcUsageRender,
    p: &mut crate::ui::core::view::ViewPainter,
    g: &UsageGeometry,
    mut y: f64,
) -> f64 {
    let UsageGeometry { cw, ch, ascent, lh, inner_x, inner_w, .. } = *g;
    let text = |p: &mut crate::ui::core::view::ViewPainter, x: f64, baseline: f64, s: &str, c: [f32; 4]| {
        p.text(x as f32, baseline as f32, s, c);
    };
    let text_w = |s: &str| -> f64 { s.chars().count() as f64 * cw as f64 };
    // ---- account cards, one row ----
    let n = cc.accounts.len().max(1);
    let gap = cw as f64 * metric::CARD_GAP;
    let card_w = ((inner_w - gap * (n as f64 - 1.0)) / n as f64).max(40.0);
    // ONE padding value for all four sides, in physical px.
    //
    // The previous cut measured horizontal padding in `cw` and vertical
    // padding in `lh`, which are different rulers — the four gaps came
    // out visibly unequal, and the bottom one collapsed to almost
    // nothing once the last row's descenders were accounted for.
    let card_pad = ch as f64 * metric::CARD_PAD;
    // Baseline-to-baseline advances between the five rows.  Named so
    // the card height below can be derived instead of guessed.
    let row_adv: Vec<f64> =
        metric::CARD_ROW_ADVANCES.iter().map(|m| lh * m).collect();

    // Exact: pad, then the first row's ascent, the row advances, the
    // last row's descent, then pad again.  `ch - ascent` is the
    // descent — the painter reports cell height and ascent, and a
    // monospace cell is exactly the two stacked.
    let window_rows = cc
        .accounts
        .iter()
        .map(|a| a.windows.len())
        .max()
        .unwrap_or(0);
    let card_h = card_height(ch as f64, ascent as f64, window_rows);
    let card_top = y;
    for (i, a) in cc.accounts.iter().enumerate() {
        let cx = inner_x + i as f64 * (card_w + gap);
        let card = Rect { x: cx, y_top: card_top, w: card_w, h: card_h };
        p.fill_rounded_rect(card, panel_palette::card_bg(), 6.0, (panel_palette::card_border(), 1.0));
        let px = cx + card_pad;
        let row_right = cx + card_w - card_pad;
        let mut cy = card_top + card_pad + ascent as f64;

        // r1 — name (primary) + status chip, right-aligned in a slot
        // reserved BEFORE the name is laid out so no length pairing can
        // make them collide.
        // The chip's TEXT is what has to line up with the percentages
        // below it — they are the same column of the card.  Aligning the
        // chip's *background* instead pushed the label half a character
        // left of the numbers, which is exactly the kind of near-miss
        // that reads as sloppy.  So: right-align the text at
        // `row_right`, then draw the pill around it, letting the pill
        // spill into the card's padding rather than moving the text.
        let chip_text = a.status_label.to_ascii_uppercase();
        let chip_pad = cw as f64 * metric::CHIP_PAD;
        let chip_text_x = row_right - text_w(&chip_text);
        // Centre the pill on the label's INK, not on its baseline box.
        // The label is all-caps, so its ink runs from `baseline - cap`
        // to the baseline with nothing below; hanging the pill off the
        // full ascent left roughly twice as much air under the letters
        // as above them.  0.72 × ascent is the usual stand-in for cap
        // height — the painter reports ascent, not cap height — and is
        // only ever applied to these fixed uppercase labels.
        let chip_h = ch as f64 * 1.05;
        let cap = ascent as f64 * metric::CAP_HEIGHT_OF_ASCENT;
        p.fill_rounded_rect(
            Rect {
                x: chip_text_x - chip_pad,
                y_top: cy - cap * 0.5 - chip_h * 0.5,
                w: text_w(&chip_text) + chip_pad * 2.0,
                h: chip_h,
            },
            panel_palette::chip_bg(a.status_severity), 3.0, ([0.0; 4], 0.0),
        );
        text(p, chip_text_x, cy, &chip_text, panel_palette::severity_color(a.status_severity));
        let name_budget = (chip_text_x - chip_pad - cw as f64 - px) / cw as f64;
        text(p, px, cy, &fit_ellipsis(&a.name, name_budget), panel_palette::fg());

        // r2 — email, one full line of its own.  Cramming it beside the
        // name is what produced the collisions; a dedicated line also
        // lets a long address show in full.
        cy += row_adv[0];
        text(p, px, cy, &fit_ellipsis(&a.email, (row_right - px) / cw as f64), panel_palette::fg_sec());

        // r3/r4 — one full-width bar per window: `5H [========----] 55%`.
        // Full width (not two half-width groups) roughly doubles the
        // resolution of the bar, which is the whole point of the panel.
        let pct_slot = cw as f64 * 4.0; // "100%"
        // The account's own windows, then one row per model cap.  The
        // model rows are the same shape on purpose: a reader should
        // not have to learn a second way to read a bar halfway down
        // the card.
        let rows: Vec<(String, f32)> =
            a.windows.iter().map(|w| (w.label.clone(), w.util)).collect();
        // Labels are no longer all two characters, so the bars start
        // after the widest one rather than at a fixed column.
        let label_cols = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(2) as f64;
        for (i, (label, util)) in rows.into_iter().enumerate() {
            // First bar row steps off the email; every later one uses
            // the tighter row-to-row advance.
            cy += if i == 0 { row_adv[1] } else { row_adv[2] };
            text(p, px, cy, &label, panel_palette::fg_faint());
            let pct = format!("{:.0}%", util * 100.0);
            text(p, row_right - text_w(&pct), cy, &pct, cc_util_color(util));
            let bar_x = px + cw as f64 * (label_cols + 1.0);
            let bar_w = (row_right - pct_slot - cw as f64 - bar_x).max(1.0);
            let bar_h = ch as f64 * metric::BAR_H;
            // Centre the bar on the text's optical middle so the row
            // reads as one unit instead of a caption above a bar.
            let bar_y = cy - ascent as f64 * 0.36 - bar_h / 2.0;
            p.fill_rounded_rect(
                Rect { x: bar_x, y_top: bar_y, w: bar_w, h: bar_h },
                panel_palette::track(), 2.0, ([0.0; 4], 0.0),
            );
            let fill_w = bar_w * util.clamp(0.0, 1.0) as f64;
            if fill_w > 0.5 {
                p.fill_rounded_rect(
                    Rect { x: bar_x, y_top: bar_y, w: fill_w, h: bar_h },
                    cc_util_color(util), 2.0, ([0.0; 4], 0.0),
                );
            }
        }

        // r5 — reset times.
        cy += row_adv[3];
        text(p, px, cy, &fit_ellipsis(&a.reset_label, (row_right - px) / cw as f64), panel_palette::fg_sec());
    }
    // Two sections, not one continuous list — give the boundary enough
    // room to read as a break.
    y = card_top + card_h + lh * metric::SECTION_BREAK;
    y
}

/// The availability timeline, starting at `y`.
fn paint_usage_timeline(
    cc: &CcUsageRender,
    p: &mut crate::ui::core::view::ViewPainter,
    g: &UsageGeometry,
    mut y: f64,
) {
    let UsageGeometry { cw, ch, ascent, lh, inner_x, inner_w, .. } = *g;
    let text = |p: &mut crate::ui::core::view::ViewPainter, x: f64, baseline: f64, s: &str, c: [f32; 4]| {
        p.text(x as f32, baseline as f32, s, c);
    };
    let text_w = |s: &str| -> f64 { s.chars().count() as f64 * cw as f64 };
    // ---- timeline ----
    p.panel_text(
        crate::ui::theme::PanelText::Title,
        inner_x as f32,
        (y + p.ui_ascent() as f64) as f32,
        "RESOURCE AVAILABILITY",
        panel_palette::fg(),
    );
    y += p.ui_line_h() as f64 + lh * metric::HEADING_GAP;
    let label_w = cc
        .accounts
        .iter()
        .map(|a| text_w(&a.name))
        .fold(0.0f64, f64::max)
        + cw as f64 * 2.0;
    let tl_x = inner_x + label_w;
    // Reserve a gutter on the right for the tags that hang off the end
    // of each window bar.  Without it the axis ran all the way to the
    // inner edge and a bar ending at (or clamped to) the right of the
    // range left its tag nowhere to go — it rendered on top of the
    // modal's own border.  Sizing the gutter to the widest tag any
    // account will draw means the plot compresses a little and nothing
    // ever has to overlap.
    let tag_gutter = cc
        .accounts
        .iter()
        .flat_map(|a| a.windows.iter().map(cc_window_tag))
        .map(|t| text_w(&t))
        .fold(0.0f64, f64::max)
        + cw as f64 * 1.2;
    let tl_w = (inner_w - label_w - tag_gutter).max(10.0);
    // Scale to what has to be drawn.  A fixed ±6 d window cut every
    // 7-day bar short (its reset is up to 7 d out), so the bar stopped
    // at the axis edge while its label still named the real reset time
    // — the bar and the number beside it disagreed.
    let extents: Vec<(i64, i64)> = cc
        .accounts
        .iter()
        .flat_map(|a| a.windows.iter())
        .filter_map(|w| w.reset_unix.map(|r| (r - w.span_secs, r)))
        .collect();
    let (t0, t1) = timeline_range(cc.now_unix, &extents);
    // Snap the range outward to local day boundaries.
    //
    // Two things at once.  It is the plot's margin — a bar no longer
    // starts or ends flush against the edge — and, more importantly,
    // it guarantees a dated rule on BOTH sides of every bar end.  The
    // range used to stop wherever the data did plus a few percent, so
    // the last bar ended past the final rule with nothing behind it to
    // read against: `7d 19% 12:00` sat to the right of `8/14` and the
    // next day was never drawn (2026-08-07 report).
    let (t0, t1) = crate::cc_usage::snap_range_to_local_days(t0, t1);
    let span_s = t1 - t0;
    let x_of = |t: f64| -> f64 { tl_x + ((t - t0) / span_s).clamp(0.0, 1.0) * tl_w };
    let bar_h = ch as f64 * metric::BAR_H;
    let bar_gap = ch as f64 * metric::TIMELINE_BAR_GAP;
    // One band per account, tall enough for that account's bars. Sized
    // off the widest account so every row is the same height — rows of
    // different heights read as different kinds of thing.
    let max_bars = cc.accounts.iter().map(|a| a.windows.len()).max().unwrap_or(2);
    let row_h = timeline_row_height(ch as f64, lh, max_bars);
    let rows_top = y;
    let rows_bottom = rows_top + cc.accounts.len() as f64 * row_h;
    // Day grid, drawn first so the bars read as sitting on top of it.
    //
    // The dates used to live only as labels under the axis, which meant
    // reading "when does Claude 3's 7d window reset" took a saccade to
    // the bottom of the chart and back.  Carrying each day up through
    // the plot as a dashed rule lets a bar's end be read against a date
    // in place.  Dashed, and faint, because it is a background
    // reference: a solid rule at this density competes with the bars,
    // and the one line that must stay solid is NOW.
    // Local days, not 86 400 s steps from a UTC-aligned start: these
    // rules are labelled with local dates below, so they have to land
    // where those dates begin.  Stepping in UTC put every rule the
    // timezone offset away from its own label — nine hours in JST, so
    // a 7-day window resetting at 00:00 on the 8th ended visibly left
    // of the rule reading `8/8`.
    let first_day = {
        let s = crate::cc_usage::local_day_start(t0 as i64);
        (if (s as f64) < t0 { crate::cc_usage::next_local_day_start(s) } else { s }) as f64
    };
    let next_day = |t: f64| crate::cc_usage::next_local_day_start(t as i64) as f64;
    let dash = ch as f64 * metric::GRID_DASH;
    let gap = ch as f64 * metric::GRID_GAP;
    let grid_top = rows_top - lh * 0.35;
    let mut t_grid = first_day;
    // `<=`: the range now ends ON a day boundary, and that last rule is
    // the one a bar ending in the final day is read against.
    while t_grid <= t1 {
        let x = x_of(t_grid);
        let mut gy = grid_top;
        while gy < rows_bottom {
            let h = dash.min(rows_bottom - gy);
            p.fill_rounded_rect(
                Rect { x, y_top: gy, w: 1.0, h },
                panel_palette::grid(), 0.0, ([0.0; 4], 0.0),
            );
            gy += dash + gap;
        }
        t_grid = next_day(t_grid);
    }
    for (i, a) in cc.accounts.iter().enumerate() {
        let ry = rows_top + i as f64 * row_h;
        // Centred on this account's whole stack of bars, whatever its
        // height — the two-bar form of this was `bar_h + bar_gap / 2`.
        let bars_h = cc_bars_height(bar_h, bar_gap, a.windows.len());
        let name_baseline = ry + bars_h / 2.0 + ascent as f64 * 0.5;
        text(p, inner_x, name_baseline, &a.name, panel_palette::fg());
        for (idx, w) in a.windows.iter().enumerate() {
            // No reset means no extent — the card still names the
            // window, but there is nothing here to draw it against.
            let Some(reset_unix) = w.reset_unix else { continue };
            let (span, reset, util) = (w.span_secs as f64, reset_unix as f64, w.util);
            let by = ry + idx as f64 * (bar_h + bar_gap);
            // Track = the rolling window's extent [reset - span,
            // reset]; the coloured fill covers only the USED portion
            // (window start + span × utilization) — "used this much
            // of this window", not "window exists".
            let wx0 = x_of(reset - span);
            let wx1 = x_of(reset);
            if wx1 - wx0 > 0.5 {
                p.fill_rounded_rect(
                    Rect { x: wx0, y_top: by, w: wx1 - wx0, h: bar_h },
                    panel_palette::track(), 2.0, ([0.0; 4], 0.0),
                );
            }
            let ux1 = x_of(reset - span + span * util.clamp(0.0, 1.0) as f64);
            if ux1 - wx0 > 0.5 {
                p.fill_rounded_rect(
                    Rect { x: wx0, y_top: by, w: ux1 - wx0, h: bar_h },
                    cc_util_color(util), 2.0, ([0.0; 4], 0.0),
                );
            }
            // Tag sits right of the window, vertically centered on
            // the bar (baseline = bar centre + half the ascent).
            let tag = cc_window_tag(w);
            // The gutter guarantees room, so this clamp is only a
            // backstop against a pathologically long reset label.
            // Strictly right of where the bar ends — never pulled back
            // over it.  The gutter above is sized to the widest tag any
            // account draws, and the axis now ends where the data ends,
            // so the furthest-right bar's tag still has its room.
            let tag_x = wx1 + cw as f64 * 0.7;
            let tag_baseline = by + bar_h / 2.0 + ascent as f64 * 0.42;
            text(p, tag_x, tag_baseline, &tag, panel_palette::fg_sec());
        }
    }
    // NOW marker.
    let nx = x_of(cc.now_unix as f64);
    p.fill_rounded_rect(
        Rect { x: nx, y_top: rows_top - lh * 0.4, w: 1.5, h: rows_bottom - rows_top + lh * 0.4 },
        panel_palette::now(), 0.0, ([0.0; 4], 0.0),
    );
    text(p, nx - text_w("NOW") / 2.0, rows_top - lh * 0.5, "NOW", panel_palette::now());
    // Date labels along the axis.  No tick stubs — each date's dashed
    // rule already lands on the axis, so a stub would just double it.
    let mut t = first_day;
    while t <= t1 {
        let x = x_of(t);
        let (mo, d, _, _) = crate::cc_usage::local_mdhm(t as i64);
        let lbl = format!("{mo}/{d}");
        text(p, x - text_w(&lbl) / 2.0, rows_bottom + lh * 0.7, &lbl, panel_palette::fg_faint());
        t = next_day(t);
    }
}

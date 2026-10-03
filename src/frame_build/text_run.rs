//! Placing a run of text: one glyph instance per glyph, in the terminal
//! face or the UI face, monospaced or shaped.  Shared by the grid, the
//! chrome and every panel painter.

use crate::font_cache::FontCache;
use crate::glyph_atlas::{AtlasEntry, GlyphAtlas, GlyphKey, SlotMetrics};
use crate::frame_build::glyph_resolve::text_glyph_key;
use crate::render_metal::{rgba8_of_f32, GlyphInstance};

/// Lay a run of text starting at baseline `(x, baseline_y)` in
/// physical pixels, advancing one monospace cell per char.  Shared
/// by the cell-title strip and the header version label.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    push_text_run_kind(
        text, x_start, baseline_y, color,
        cell_w, cell_h, ascent, atlas_w, atlas_h,
        font, atlas, glyphs, FontKind::Terminal,
    )
}

/// Which font family/path to use for text rendering.  `Terminal` =
/// mono cell-aligned (PTY grid).  `Ui` = system UI font (SF Pro on
/// macOS), proportional;  per-glyph advance via CT, falls back to
/// mono cascade for chars the UI font lacks (CJK).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind { Terminal, Ui }

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run_kind(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
    kind: FontKind,
) {
    // Phase 3 — chrome runs go through `FontCache::shape_ui` (CTLine
    // shaping with cached re-shape, kerning + ligatures + auto font
    // fallback ON).  PTY runs keep the mono cell loop below.  Phase 5
    // weight is plumbed via `push_text_run_kind_weighted` — this
    // legacy entry point keeps the regular-weight (400) default for
    // back-compat callers.
    if kind == FontKind::Ui {
        // Legacy entry: no colour atlas / sink in scope, so colour
        // emoji glyphs fall back to mono alpha silhouettes (the
        // pre-Phase-7 behaviour).  Chrome calls
        // `push_text_run_ui_shaped` directly with both atlases.
        // Phase 8 — legacy entry has no `opts` either; default to
        // `full()` so existing callers preserve CTLine defaults.
        push_text_run_ui_shaped_mono(
            text, x_start, baseline_y, color,
            ascent, atlas_w, atlas_h, 400,
            font, atlas, glyphs,
        );
        return;
    }
    let metrics = SlotMetrics {
        cell_w: cell_w.round() as u32,
        cell_h: cell_h.round() as u32,
        baseline_from_top: ascent.round() as u32,
    };
    let mut x = x_start;
    for ch in text.chars() {
        let n_cells = crate::grid::char_width(ch).max(1) as u16;
        let (font_idx, glyph) = font.resolve_char(ch, false, false);
        if glyph != 0 {
            if let Some(entry) = atlas.get_or_rasterize(
                text_glyph_key(font_idx as u32, glyph, font.font_pt_size(font_idx)),
                metrics,
                n_cells,
            ) {
                // Phase 1.1 bearing formula.  Pre-round to the integer
                // slot-top grid the pre-Phase-1.1 code used (so Phase 1.0
                // entries are bit-equivalent to the old `dest_y =
                // (baseline_y - ascent).round()` formula).
                let baseline_from_top_f = metrics.baseline_from_top as f32;
                let baseline_y_q =
                    (baseline_y - baseline_from_top_f).round() + baseline_from_top_f;
                let (origin, size) = entry.quad(x.round(), baseline_y_q);
                glyphs.push(GlyphInstance {
                    origin,
                    size,
                    uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
                    uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
                    color: rgba8_of_f32(color),
                });
            }
        }
        x += cell_w * n_cells as f32;
    }
}

/// Phase 3 — chrome `Ui` text run.  Shapes the line through CTLine
/// (cached by `FontCache::shape_ui_weighted`), then for each shaped
/// glyph allocates an atlas slot via `get_or_rasterize_natural` (no
/// cell-fit fallback — glyph bbox sized) and emits a `GlyphInstance`
/// at the typographic origin CTLine gave us.  ASCII gets real
/// kerning (`Ta` reads tight); `fi` / `==>` show ligatures; CJK in a
/// Latin sentence routes through PingFang / Hiragino automatically.
///
/// Phase 5 — `weight` carries the CSS weight (100..900); `400` reuses
/// the base UI font, other values materialise the variable-font
/// weight variant on first call.
/// Phase 7 — mono-only chrome shape path.  Same as
/// `push_text_run_ui_shaped` but no colour atlas / sink in scope, so
/// colour-emoji glyphs fall back to the mono atlas (alpha silhouette
/// — pre-Phase-7 visual).  Used by the legacy `push_text_run_kind`
/// entry point that pre-dates the canvas colour glyph plumbing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run_ui_shaped_mono(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    weight: u16,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    let shaped = font.shape_ui_weighted(text, weight);
    if shaped.is_empty() {
        return;
    }
    let baseline_y_q = baseline_y.round();
    let x_start_floor = x_start.floor() as i32;
    for sg in shaped {
        let key = GlyphKey::new(
            sg.font_id,
            sg.glyph_id,
            GlyphKey::size_q_for(font.font_pt_size(sg.font_id as usize)),
            sg.subpx_x,
            GlyphKey::FLAG_SMOOTH,
        );
        let Some(entry) = atlas.get_or_rasterize_natural(key) else {
            continue;
        };
        let pen_x = (x_start_floor + sg.pen_x_px) as f32;
        let (origin, size) = entry.quad(pen_x, baseline_y_q);
        glyphs.push(GlyphInstance {
            origin,
            size,
            uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
            uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
            color: rgba8_of_f32(color),
        });
    }
    let _ = ascent;
}

/// SF Pro at an explicit pt size and weight, into the mono atlas.
///
/// [`push_text_run_ui_shaped_mono`] above is locked to the startup
/// `UI_FONT_POINT` at weight 600 — one size, one weight, which is why
/// every chrome surface built on `ViewPainter` had a single type size
/// and had to signal hierarchy with colour alone.  The canvas path
/// (`push_text_run_ui_shaped`) has taken a size since Phase 10c; this
/// is the same capability for the direct painter, minus the colour
/// atlas the canvas path also routes (chrome labels are latin).
///
/// `baseline_y` is a real baseline in physical px — the caller knows
/// its own line box.  Physical px per pt is the 2× retina scale baked
/// into the atlas raster path, same as the canvas path assumes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_text_run_ui_sized(
    text: &str,
    x_start: f32,
    baseline_y: f32,
    color: [f32; 4],
    size_pt: f64,
    weight: u16,
    atlas_w: f32,
    atlas_h: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
) {
    let shaped = font.shape_ui_weighted_opts_at_size(
        text, weight, crate::font_shape::ShapeOptions::default(), size_pt,
    );
    if shaped.is_empty() {
        return;
    }
    let baseline_y_q = baseline_y.round();
    let x_start_floor = x_start.floor() as i32;
    for sg in shaped {
        let key = GlyphKey::new(
            sg.font_id,
            sg.glyph_id,
            GlyphKey::size_q_for(font.font_pt_size(sg.font_id as usize)),
            sg.subpx_x,
            GlyphKey::FLAG_SMOOTH,
        );
        let Some(entry) = atlas.get_or_rasterize_natural(key) else {
            continue;
        };
        let pen_x = (x_start_floor + sg.pen_x_px) as f32;
        let (origin, size) = entry.quad(pen_x, baseline_y_q);
        glyphs.push(GlyphInstance {
            origin,
            size,
            uv0: [entry.u0 as f32 / atlas_w, entry.v0 as f32 / atlas_h],
            uv1: [entry.u1 as f32 / atlas_w, entry.v1 as f32 / atlas_h],
            color: rgba8_of_f32(color),
        });
    }
}

#[allow(clippy::too_many_arguments)]
// y_top_phys = top-of-em-box y in PHYSICAL px (raw `t.y` from canvas
// Length resolution, NOT a pre-baked baseline).  Function derives the
// baseline from the SF Pro run's own ascent at the requested pt size,
// so callers don't need to know chrome cell ascent.
// fallback_ascent = chrome cell ascent (phys px) — used ONLY when
// ui_size_q.is_none() (pre-Phase-10c default-size callers).
pub(crate) fn push_text_run_ui_shaped(
    text: &str,
    x_start: f32,
    y_top_phys: f32,
    color: [f32; 4],
    fallback_ascent: f32,
    atlas_w: f32,
    atlas_h: f32,
    color_atlas_w: f32,
    color_atlas_h: f32,
    weight: u16,
    opts: crate::font_shape::ShapeOptions,
    // Phase 10c — `None` = use FontCache's default UI_FONT_POINT;
    // `Some(q)` = use SF Pro at `q / 4.0` pt.
    ui_size_q: Option<u16>,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
) {
    let shaped = match ui_size_q {
        Some(q) => font.shape_ui_weighted_opts_at_size(text, weight, opts, (q as f64) / 4.0),
        None => font.shape_ui_weighted_opts(text, weight, opts),
    };
    if shaped.is_empty() {
        return;
    }
    // SF Pro path uses the run's OWN ascent — derived from the first
    // shaped glyph's CTFont — so positioning a Body 8.1pt run reads
    // as 8.1pt top-of-em, not chrome cell pitch.  Bug fix 2026-06-25:
    // previously `baseline_y` was pre-computed with chrome ascent,
    // misaligning active-row BG vs SF Pro glyphs.
    let real_ascent_pt = font.font_ascent(shaped[0].font_id as usize);
    // CTFont.ascent() returns pt — convert to phys at the 2× retina
    // baked into the atlas raster path (`RETINA_SCALE = 2.0`).
    let real_ascent_phys = (real_ascent_pt * 2.0) as f32;
    let baseline_y = if ui_size_q.is_some() {
        y_top_phys + real_ascent_phys
    } else {
        y_top_phys + fallback_ascent
    };
    let baseline_y_q = baseline_y.round();
    let x_start_floor = x_start.floor() as i32;
    for sg in shaped {
        // Phase 7 — colour vs mono routing.  CT may have fallen back
        // to Apple Color Emoji for any glyph in the run; rasterising
        // those into the R8 atlas would emit an alpha silhouette
        // (no colour) so the caller would see a black emoji shape.
        // Routing to `color_atlas` (BGRA8) emits real colours, and
        // the matching `fg_color_pipeline` pass blends them in
        // submission order with the mono runs.
        let is_color = font.is_color_font(sg.font_id as usize);
        // Phase 4 — `sg.subpx_x` bucket comes from CTLine's float
        // position (shape_line quantised it).  Atlas hands back a slot
        // whose ink is pre-shifted by `subpx_x × 0.25 px`, so
        // origin.x stays integer.
        let key = GlyphKey::new(
            sg.font_id,
            sg.glyph_id,
            GlyphKey::size_q_for(font.font_pt_size(sg.font_id as usize)),
            sg.subpx_x,
            GlyphKey::FLAG_SMOOTH,
        );
        let (entry_opt, aw, ah, sink): (Option<AtlasEntry>, f32, f32, &mut Vec<GlyphInstance>) =
            if is_color {
                (
                    color_atlas.get_or_rasterize_natural(key),
                    color_atlas_w,
                    color_atlas_h,
                    color_glyphs,
                )
            } else {
                (
                    atlas.get_or_rasterize_natural(key),
                    atlas_w,
                    atlas_h,
                    glyphs,
                )
            };
        let Some(entry) = entry_opt else {
            continue;
        };
        let pen_x = (x_start_floor + sg.pen_x_px) as f32;
        let (origin, size) = entry.quad(pen_x, baseline_y_q);
        sink.push(GlyphInstance {
            origin,
            size,
            uv0: [entry.u0 as f32 / aw, entry.v0 as f32 / ah],
            uv1: [entry.u1 as f32 / aw, entry.v1 as f32 / ah],
            color: rgba8_of_f32(color),
        });
    }
    let _ = fallback_ascent;
}

/// Where an IME preedit's clusters land: `(row, col, width, cluster)`,
/// in draw order.
///
/// Pure, and the ONE answer to "which cells does the composition
/// occupy".  Both the mask that hides the cells underneath and the
/// drawing itself read it, because the two disagreeing is exactly the
/// bug this was extracted for: BG and FG are separate passes, so a
/// preedit that paints an opaque background still has the terminal's
/// own glyphs drawn over it afterwards unless those glyphs are left
/// out.  That is the same failure the search overlay's mask fixed
/// (F1++) — the composition never got one, so a claudecode
/// placeholder showed through the pinyin being typed over it
/// (2026-09-23).
///
/// Clusters, not chars (UAX #29), so a CJK syllable plus its tone
/// mark or an emoji ZWJ sequence takes its real cell footprint.
/// Wraps at the right edge like iTerm2 / Alacritty rather than
/// truncating, and stops at the bottom row — the IME's own candidate
/// window remains the source of truth for a composition that long.
pub(crate) fn preedit_placements(
    preedit: &str,
    cursor: (u16, u16),
    cols: u16,
    rows: u16,
) -> Vec<(u16, u16, u16, &str)> {
    let (col, row) = cursor;
    let mut out = Vec::new();
    if cols == 0 || rows == 0 {
        return out;
    }
    let (mut c, mut r) = (col as u32, row as u32);
    for cluster in crate::grapheme::graphemes(preedit) {
        // Newlines from an IME's structured composition: advance to
        // the next row at col 0, with no glyph of their own.
        if cluster == "\n" || cluster == "\r" || cluster == "\r\n" {
            c = 0;
            r = r.saturating_add(1);
            if r >= rows as u32 {
                break;
            }
            continue;
        }
        let n_cells = crate::grapheme::cluster_width(cluster).max(1) as u32;
        if c + n_cells > cols as u32 {
            c = 0;
            r = r.saturating_add(1);
            if r >= rows as u32 {
                break;
            }
        }
        out.push((r as u16, c as u16, n_cells as u16, cluster));
        c += n_cells;
    }
    out
}

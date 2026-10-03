//! Which atlas entry a cell is drawn with.
//!
//! A cell's character, attributes and cluster text go in; an atlas entry
//! comes out, with box-drawing and block elements routed to our own
//! cell-exact masks and everything else to the font.  Nothing here knows
//! what a graphics API is.

use core_graphics::font::CGGlyph;

use crate::font_cache::FontCache;
use crate::glyph_atlas::{AtlasEntry, GlyphAtlas, GlyphKey, SlotMetrics, BOX_DRAWING_FONT_ID};
use crate::render::{box_drawing_arms, block_element_rects, rasterize_arms_into_buf, rasterize_block_into_buf};

/// Resolve an arbitrary cell character to an atlas entry, routing
/// box-drawing (U+2500-U+257F + ╭╮╯╰) and block elements
/// (U+2580-U+259F) through our own pixel-perfect mask rasteriser
/// instead of CT's font glyph.  CT glyphs for these chars typically
/// don't span the cell advance, producing visible seams when
/// claudecode / vim / htop draw box borders or progress bars; our
/// masks fill the cell exactly so adjacent cells join with zero
/// drift.  Pure Rust path, no extra deps, atlas + GPU pipeline
/// downstream is unchanged.
/// An atlas entry for a whole grapheme cluster.
///
/// The cluster is rasterised once, by CoreText, into a cell-sized
/// bitmap and cached under a hash of its text — so a screen full of
/// the same cluster costs one raster, and a cluster that never recurs
/// costs one that the atlas evicts like any other.
///
/// The font is chosen by the cluster's base codepoint, which is the
/// same choice the old single-glyph path made for that cell; what
/// changes is that the mark, the ZWJ tail or the second regional
/// indicator now gets drawn with it.
pub(crate) fn resolve_cluster_glyph(
    atlas: &mut GlyphAtlas,
    font: &mut FontCache,
    text: &str,
    bold: bool,
    italic: bool,
    metrics: SlotMetrics,
) -> Option<AtlasEntry> {
    let base = marspot_term::grapheme::cluster_first_codepoint(text);
    let (font_idx, _) = font.resolve_char(base, bold, italic);
    let n_cells = marspot_term::grapheme::cluster_width(text).max(1) as u16;
    let key = crate::glyph_atlas::cluster_key(text, &metrics);
    let w = metrics.cell_w * n_cells as u32;
    let h = metrics.cell_h;
    let mut drew = false;
    let entry = atlas.get_or_insert_custom_raster(
        key,
        w,
        h,
        metrics.baseline_from_top,
        n_cells,
        |buf| {
            // Resolved here rather than before the call: the closure
            // only runs on a miss, and this is a `CFRetain`.
            drew = match font.font(font_idx) {
                Some(f) => {
                    crate::glyph_atlas::rasterise_cluster(text, &f, metrics, n_cells, buf)
                }
                None => false,
            };
        },
    );
    let _ = drew;
    entry
}

pub(crate) fn resolve_cell_glyph(
    atlas: &mut GlyphAtlas,
    font: &mut FontCache,
    ch: char,
    bold: bool,
    italic: bool,
    metrics: SlotMetrics,
) -> Option<AtlasEntry> {
    if let Some(arms) = box_drawing_arms(ch) {
        let w = metrics.cell_w;
        let h = metrics.cell_h;
        let key = box_drawing_key(ch, &metrics);
        return atlas.get_or_insert_custom_raster(key, w, h, metrics.baseline_from_top, 1, |buf| {
            rasterize_arms_into_buf(buf, w as usize, h as usize, arms);
        });
    }
    if let Some(shape) = block_element_rects(ch) {
        let w = metrics.cell_w;
        let h = metrics.cell_h;
        let key = box_drawing_key(ch, &metrics);
        return atlas.get_or_insert_custom_raster(key, w, h, metrics.baseline_from_top, 1, |buf| {
            rasterize_block_into_buf(buf, w as usize, h as usize, shape);
        });
    }
    let (font_idx, glyph) = font.resolve_char(ch, bold, italic);
    if glyph == 0 {
        return None;
    }
    let n_cells = crate::grid::char_width(ch).max(1) as u16;
    atlas.get_or_rasterize(
        text_glyph_key(font_idx as u32, glyph, font.font_pt_size(font_idx)),
        metrics,
        n_cells,
    )
}

/// Phase 2 — synthesise an atlas key for a box-drawing / block-element
/// glyph.  These are rasterised by our own code (not CT), so they have
/// no `pt_size` — but cell dimensions stand in for "rendering size", so
/// a glyph rasterised at cell_h = 32 vs cell_h = 24 lands in different
/// slots.  Pack `cell_h` into `size_q` to keep cross-DPI rasters
/// separated.  `flags = 0` since the custom rasters don't use the CT
/// smoothing knobs.
#[inline]
pub(crate) fn box_drawing_key(ch: char, metrics: &SlotMetrics) -> GlyphKey {
    GlyphKey::new(
        BOX_DRAWING_FONT_ID,
        ch as u32 as CGGlyph,
        metrics.cell_h as u16,
        0,
        0,
    )
}

/// Phase 2 — atlas key for a CT-rasterised text glyph.  `size_q`
/// quantises the font's pt-size to 0.25-pt buckets so PTY 12pt and
/// chrome 13pt cache independently.  `subpx_x` reserved for Phase 4.
/// `flags = FLAG_SMOOTH` because every call into `rasterise_glyph`
/// runs with font-smoothing on (see the `set_should_smooth_fonts(true)`
/// line in the rasteriser).
#[inline]
/// `pt_size` comes from the `FontCache` snapshot, not from a font
/// object: this runs once per cell per frame, and asking a `CTFont`
/// for its point size meant cloning one (a `CFRetain`) to ask it a
/// constant.
pub(crate) fn text_glyph_key(font_id: u32, glyph: CGGlyph, pt_size: f64) -> GlyphKey {
    GlyphKey::new(
        font_id,
        glyph,
        GlyphKey::size_q_for(pt_size),
        0,
        GlyphKey::FLAG_SMOOTH,
    )
}

// How wide a cell's glyph is drawn is decided by the width table
// alone — never by what happens to sit next to it.
//
// There was a rule here that let a squeezed glyph spill into the cell
// on its right when that cell was blank (WezTerm's
// `WhenFollowedBySpace`).  It is gone: `①` came out full-size before a
// space and small before a character, so the *same* character changed
// size as the line around it was written, which reads as a rendering
// fault whichever size you preferred.  A cell now looks the way it
// looks because of what is in it.
//
// The circled family is genuinely too wide for one cell — PingFang
// draws `①` 11.71 px against a 7.20 px cell — so one cell means
// scaled down, always.  Full size costs a second cell, which moves
// the wrap point; that is a decision with a real downside, so it is
// the settings panel's `appearance_circled_wide`, off by default.

/// Like `resolve_cell_glyph`, but routes colour glyphs (Apple Color Emoji)
/// to the colour (`BGRA8`) atlas and everything else to the mono (`R8`)
/// atlas.  Returns `(entry, is_color)` so the caller can pick the matching
/// glyph buffer + atlas dims.  Box-drawing / block-element glyphs are always
/// mono (we rasterise those ourselves).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_cell_glyph_routed(
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    font: &mut FontCache,
    ch: char,
    bold: bool,
    italic: bool,
    metrics: SlotMetrics,
) -> Option<(AtlasEntry, bool)> {
    if box_drawing_arms(ch).is_some() || block_element_rects(ch).is_some() {
        return resolve_cell_glyph(atlas, font, ch, bold, italic, metrics).map(|e| (e, false));
    }
    let (font_idx, glyph) = font.resolve_char(ch, bold, italic);
    if glyph == 0 {
        return None;
    }
    let key = text_glyph_key(font_idx as u32, glyph, font.font_pt_size(font_idx));
    let n_cells = crate::grid::char_width(ch).max(1) as u16;
    if font.is_color_font(font_idx) {
        color_atlas
            .get_or_rasterize(key, metrics, n_cells)
            .map(|e| (e, true))
    } else {
        atlas
            .get_or_rasterize(key, metrics, n_cells)
            .map(|e| (e, false))
    }
}

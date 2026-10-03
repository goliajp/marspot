//! One frame's panes, each either copied from what it drew last frame
//! or laid out again when anything it reads has changed.

use crate::font_cache::FontCache;
use crate::frame_build::session::push_session;
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::{CellRect, Layout};
use crate::render::SessionView;
use golia_ui_core::scene::{RectInstance as CellInstance, GlyphInstance, UiRectInstance};
use crate::frame_build::frame::BuildStats;

/// F1+13 — cached per-pane render contributions.  When a pane's
/// `fingerprint` (computed from its `SessionView` fields) and both
/// atlas generations match the previous frame, the renderer
/// `extend_from_slice`s the cached vecs straight into the current
/// frame's accumulators — skipping `push_session` for that pane
/// entirely.  Cache is invalidated whenever an atlas was rebuilt
/// or any contributing input changed.
#[derive(Default)]
pub(crate) struct PaneInstanceCache {
    fingerprint: u64,
    atlas_gen: u64,
    color_atlas_gen: u64,
    /// `link_probe::generation()` at the time this slot was built.
    /// Link verdicts land asynchronously, so a pane whose grid did
    /// not change still has to rebuild once when new answers arrive
    /// — otherwise a resolved file link never gets underlined until
    /// something else dirties the pane.
    link_gen: u64,
    cells: Vec<CellInstance>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
    /// `true` once this slot has actually been built at least once;
    /// distinguishes "fresh default" from "valid but happens to
    /// have empty contributions".
    primed: bool,
}

/// Compute the fingerprint hash of the inputs to push_session that
/// affect rendered output.  Any change here invalidates the per-pane
/// instance cache and forces a rebuild.  Cheap (~tens of ns) so it's
/// run unconditionally each frame.
/// Everything `push_session` reads, and nothing else.
///
/// The toolbar's hover index used to be hashed in here.  It is window
/// state, not pane state — `push_session` does not take it and cannot
/// see it — so moving the mouse across the toolbar changed every
/// pane's key at once and rebuilt all of them, in a window where
/// nothing about any pane had changed.  Whatever goes in here has to
/// be something the cached bytes actually depend on.
fn pane_fingerprint(
    view: &SessionView,
    rect: &CellRect,
    window_focused: bool,
    cell_w: f32,
    cell_h: f32,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = marspot_term::fast_hash::FxHasher::default();
    view.seq.hash(&mut h);
    view.view_offset.hash(&mut h);
    view.cursor_visible.hash(&mut h);
    view.focused.hash(&mut h);
    view.title.hash(&mut h);
    view.right_badge.hash(&mut h);
    view.update_pending.hash(&mut h);
    view.ime_preedit.hash(&mut h);
    view.top_fixed_h_cells.hash(&mut h);
    view.bot_fixed_h_cells.hash(&mut h);
    // Selection
    if let Some(sel) = view.selection {
        true.hash(&mut h);
        sel.anchor.0.hash(&mut h);
        sel.anchor.1.hash(&mut h);
        sel.focus.0.hash(&mut h);
        sel.focus.1.hash(&mut h);
        sel.blockwise.hash(&mut h);
    } else {
        false.hash(&mut h);
    }
    // Highlight spans (search active hit).
    view.highlight_spans.len().hash(&mut h);
    for span in view.highlight_spans {
        span.view_row.hash(&mut h);
        span.col_start.hash(&mut h);
        span.col_end_inclusive.hash(&mut h);
    }
    // Search overlay snapshot — every field affecting paint.
    if let Some(ov) = view.search_overlay.as_ref() {
        true.hash(&mut h);
        ov.query.hash(&mut h);
        ov.query_cursor.hash(&mut h);
        ov.case_sensitive.hash(&mut h);
        ov.counter.hash(&mut h);
        ov.hits.len().hash(&mut h);
        for hit in &ov.hits {
            hit.is_focused.hash(&mut h);
            hit.snippet.hash(&mut h);
        }
    } else {
        false.hash(&mut h);
    }
    window_focused.hash(&mut h);
    // Layout (catches resize → cache invalidation naturally).
    (rect.x as i64).hash(&mut h);
    (rect.y_top as i64).hash(&mut h);
    (rect.w as i64).hash(&mut h);
    (rect.h as i64).hash(&mut h);
    // Font metrics (catches font / DPI change).
    (cell_w as i64).hash(&mut h);
    (cell_h as i64).hash(&mut h);
    h.finish()
}

/// Lay out every pane that has a seat in `layout`, reusing a pane's
/// instances from `pane_caches` when nothing it reads has changed.
pub(crate) fn push_panes(
    layout: &Layout,
    views: &[SessionView],
    window_focused: bool,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    atlas_w_f: f32,
    atlas_h_f: f32,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    ui_rects: &mut Vec<UiRectInstance>,
    pane_caches: &mut Vec<PaneInstanceCache>,
) -> BuildStats {
    // Ensure cache has a slot per pane (grown lazily; never shrunk
    // intra-session — pane count is bounded by the 9-grid layout).
    while pane_caches.len() < views.len() {
        pane_caches.push(PaneInstanceCache::default());
    }

    let t_panes0 = std::time::Instant::now();
    let mut rebuilt = 0u32;
    let mut considered = 0u32;
    for (i, view) in views.iter().enumerate() {
        let rect = match layout.cells.get(i) {
            Some(r) => r,
            None => continue,
        };
        considered += 1;
        // F1+13 — per-pane instance cache.  Hash the inputs that
        // affect `push_session`'s output.  Hit ⇒ memcpy cached
        // slices into the global accumulators (cheap).  Miss ⇒
        // rebuild + snapshot the slice this pane just produced
        // into the cache so the NEXT idle frame for this pane is
        // a hit.  ui_rects are NOT cached (only one pane has the
        // search overlay at a time, and rebuilding it is cheap).
        let fp = pane_fingerprint(view, rect, window_focused, cell_w, cell_h);
        let cur_atlas_gen = atlas.rebuild_count;
        let cur_color_gen = color_atlas.rebuild_count;
        let cur_link_gen = crate::link_probe::generation();
        let cache = &mut pane_caches[i];
        let hit = cache.primed
            && cache.fingerprint == fp
            && cache.atlas_gen == cur_atlas_gen
            && cache.color_atlas_gen == cur_color_gen
            && cache.link_gen == cur_link_gen;
        if hit {
            cells.extend_from_slice(&cache.cells);
            glyphs.extend_from_slice(&cache.glyphs);
            color_glyphs.extend_from_slice(&cache.color_glyphs);
            continue;
        }
        rebuilt += 1;
        let cells_start = cells.len();
        let glyphs_start = glyphs.len();
        let color_glyphs_start = color_glyphs.len();
        push_session(
            rect,
            view,
            window_focused,
            cell_w,
            cell_h,
            ascent,
            atlas_w_f,
            atlas_h_f,
            font,
            atlas,
            color_atlas,
            cells,
            glyphs,
            color_glyphs,
            ui_rects,
            layout.gutter as f32,
            layout.padding as f32,
            layout.cell_title_h as f32,
        );
        // Snapshot this pane's contributions into the cache.
        // Re-read atlas gens after the call: a glyph miss during
        // push_session may have triggered a rebuild, in which case
        // the slice we're caching uses the post-rebuild uvs and
        // must record THAT gen for the hit check to be sound.
        let cache = &mut pane_caches[i];
        cache.fingerprint = fp;
        cache.atlas_gen = atlas.rebuild_count;
        cache.color_atlas_gen = color_atlas.rebuild_count;
        cache.link_gen = cur_link_gen;
        cache.cells.clear();
        cache.cells.extend_from_slice(&cells[cells_start..]);
        cache.glyphs.clear();
        cache.glyphs.extend_from_slice(&glyphs[glyphs_start..]);
        cache.color_glyphs.clear();
        cache.color_glyphs.extend_from_slice(&color_glyphs[color_glyphs_start..]);
        cache.primed = true;
    }
    let panes_us = t_panes0.elapsed().as_micros() as u64;
    BuildStats { panes_us, rebuilt, considered }
}

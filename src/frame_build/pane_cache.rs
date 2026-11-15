//! One frame's panes.  A pane nothing has changed for is left as it
//! was drawn; one that has changed lays out again only the rows whose
//! cells, or whatever else they draw from, are different.
//!
//! A pane's instances are kept as three lists (cell rectangles, glyphs,
//! colour glyphs), each laid out head, row 0, row 1, ..., tail: the head
//! is the pane's background, the tail what is drawn
//! over the grid (selection, link underlines, cursor, IME composition).
//! That is the order they were always drawn in, so a pane patched row by
//! row holds exactly what laying it out from nothing would.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};

use crate::font_cache::FontCache;
use crate::frame_build::frame::BuildStats;
use crate::frame_build::relayout::{relayout, Span};
use crate::frame_build::session::{
    paint_head, paint_row, paint_tail, pane_geom, LinkBufs, PaneGeom, PaneMetrics, PaneOut,
};
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::{CellRect, Layout};
use crate::render::SessionView;
use golia_ui_core::scene::{GlyphInstance, RectInstance as CellInstance, UiRectInstance};
use marspot_term::fast_hash::FxHasher;

/// Where something ends in each of the three lists, in instances.
type Ends = [u32; 3];

#[derive(Clone, Copy, Default)]
struct RowSlot {
    key: u64,
    end: Ends,
}

#[derive(Default)]
pub(crate) struct PaneInstanceCache {
    fingerprint: u64,
    /// `link_probe::generation()` at the time this slot was built.
    /// Link verdicts land asynchronously, so a pane whose grid did
    /// not change still has to look again when new answers arrive.
    link_gen: u64,
    /// What every row's instances are made from besides the row itself.
    frame_key: u64,
    pub(crate) cells: Vec<CellInstance>,
    pub(crate) glyphs: Vec<GlyphInstance>,
    pub(crate) color_glyphs: Vec<GlyphInstance>,
    head_end: Ends,
    rows: Vec<RowSlot>,
    links: LinkBufs,
    primed: bool,
    /// Is this pane in the frame last laid out?
    pub(crate) shown: bool,
    /// Per list (cells, glyphs, colour glyphs), the runs that changed
    /// since whoever keeps a copy of the lists last emptied this, in
    /// order.  Bounded: past a few dozen runs it is one run covering
    /// the list.
    pub(crate) dirty: [Vec<Span>; 3],
}


/// The rows laid out again while a pane was patched, held until the
/// lists are rearranged around them.  Kept between frames so patching
/// allocates nothing; a pane that changes by more than half is laid out
/// in place instead, so this never holds more than half a pane.
#[derive(Default)]
struct Patch {
    cells: Vec<CellInstance>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
    /// One per piece of the pane, head first and tail last: where its
    /// new instances end in the lists above, or `None` when it is kept.
    pieces: Vec<Option<Ends>>,
    keys: Vec<u64>,
    row_clusters: Vec<(u16, String)>,
    /// `relayout`'s three tables for the list being rearranged.
    old_ends: Vec<u32>,
    new_ends: Vec<u32>,
    fresh_ends: Vec<Option<u32>>,
}

thread_local! {
    static PATCH: RefCell<Patch> = RefCell::new(Patch::default());
}

/// Whether anything at all `paint_*` reads for this pane has changed.
/// Cheap, so it is asked every frame; only a pane that fails it looks
/// at its rows.
///
/// The toolbar's hover index used to be hashed in here.  It is window
/// state, not pane state, so moving the mouse across the toolbar
/// changed every pane's key at once.  Whatever goes in here has to be
/// something the cached instances actually depend on.
fn pane_fingerprint(view: &SessionView, window_focused: bool, frame_key: u64) -> u64 {
    let mut h = FxHasher::default();
    frame_key.hash(&mut h);
    view.seq.hash(&mut h);
    view.cursor_visible.hash(&mut h);
    view.focused.hash(&mut h);
    view.ime_preedit.hash(&mut h);
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
    h.finish()
}

/// What every row of a pane is laid out from besides its own cells:
/// where the pane is, the measures, the theme, the atlases.  A change
/// here lays the whole pane out again.
fn frame_key(
    view: &SessionView,
    rect: &CellRect,
    m: &PaneMetrics,
    theme: marspot_term::palette::ThemeId,
    atlas: &GlyphAtlas,
    color_atlas: &GlyphAtlas,
) -> u64 {
    let mut h = FxHasher::default();
    // Every colour in the cached instances is the theme's.
    theme.hash(&mut h);
    // Which font draws a Han code point follows the interface language.
    marspot_term::language::version().hash(&mut h);
    view.view_offset.hash(&mut h);
    view.top_fixed_h_cells.hash(&mut h);
    view.bot_fixed_h_cells.hash(&mut h);
    view.grid.cols().hash(&mut h);
    view.grid.rows().hash(&mut h);
    for v in [rect.x, rect.y_top, rect.w, rect.h] {
        v.to_bits().hash(&mut h);
    }
    for v in [m.cell_w, m.cell_h, m.ascent, m.gutter, m.padding, m.title_h] {
        v.to_bits().hash(&mut h);
    }
    // An atlas rebuilt or a shelf recycled moves glyphs: instances
    // made before it point at the wrong place.
    atlas.rebuild_count.hash(&mut h);
    atlas.evictions.hash(&mut h);
    color_atlas.rebuild_count.hash(&mut h);
    color_atlas.evictions.hash(&mut h);
    h.finish()
}

/// Everything `paint_row` reads for row `r`: its cells, the text of
/// its clusters, and whichever of the cursor, the search hits, the
/// search panel, an IME composition and the detected links touch it.
fn row_key(g: &PaneGeom, r: u16, row_clusters: &mut Vec<(u16, String)>) -> u64 {
    let view = g.view;
    let grid = g.grid;
    let mut h = FxHasher::default();
    // a live row in one pass over the grid's own cells: a scrolling
    // pane changes every row every frame, and a call and five writes a
    // cell cost more than the rows it would save
    if grid.cols() as usize != g.cols || !grid.hash_live_row(view.view_offset, r, &mut h) {
        for c in 0..g.cols as u16 {
            let cell = grid.cell_at_view(view.view_offset, c, r);
            // SAFETY: a `Cell` is twenty bytes, aligned to four, with no
            // padding and every byte initialised (the grid asserts that
            // layout); five words are exactly those bytes.
            let words: [u32; 5] = unsafe { std::mem::transmute(cell) };
            for w in words {
                h.write_u32(w);
            }
            if let Some(text) = grid.cluster_text(&cell) {
                text.hash(&mut h);
            }
        }
    }
    // a row in history keeps its clusters' text beside it, not in the cells
    if view.view_offset != 0 && grid.row_clusters_at_view(view.view_offset, r, row_clusters) {
        for (col, text) in row_clusters.iter() {
            col.hash(&mut h);
            text.hash(&mut h);
        }
    }
    for span in view.highlight_spans.iter().filter(|s| s.view_row == r) {
        h.write_u32(1);
        span.col_start.hash(&mut h);
        span.col_end_inclusive.hash(&mut h);
    }
    let cursor = grid.cursor();
    if cursor.1 == r && view.view_offset == 0 && view.cursor_visible && view.focused && g.window_focused {
        h.write_u32(2);
        cursor.0.hash(&mut h);
    }
    if let Some(mask) = g.overlay_mask
        && r >= mask.0
        && r < mask.1
    {
        h.write_u32(3);
        mask.hash(&mut h);
    }
    for &(row, col, width, _) in g.preedit_cells.iter().filter(|p| p.0 == r) {
        h.write_u32(4);
        (row, col, width).hash(&mut h);
    }
    for link in g.links.iter().filter(|l| l.row == r) {
        h.write_u32(5);
        link.col_start.hash(&mut h);
        link.col_end.hash(&mut h);
    }
    h.finish().max(UNKNOWN_KEY + 1)
}

/// Lay out every pane that has a seat in `layout`.  With `inline`, a
/// pane's instances are also appended to `cells`, `glyphs` and
/// `color_glyphs`; without it they stay in `pane_caches` for a caller
/// that draws each pane from its own buffers.
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
    _ui_rects: &mut Vec<UiRectInstance>,
    pane_caches: &mut Vec<PaneInstanceCache>,
    inline: bool,
) -> BuildStats {
    // Ensure cache has a slot per pane (grown lazily; never shrunk
    // intra-session — pane count is bounded by the 9-grid layout).
    while pane_caches.len() < views.len() {
        pane_caches.push(PaneInstanceCache::default());
    }
    for cache in pane_caches.iter_mut() {
        cache.shown = false;
    }

    // The one place a frame asks which theme is in force for what it
    // draws inside a pane; everything below is handed the answer.
    let colors = crate::ui::theme::terminal::TermColors::current();
    let t_panes0 = std::time::Instant::now();
    let mut stats = BuildStats::default();
    for (i, view) in views.iter().enumerate() {
        let rect = match layout.cells.get(i) {
            Some(r) => r,
            None => continue,
        };
        stats.considered += 1;
        let (color_atlas_w, color_atlas_h) = color_atlas.dims();
        let m = PaneMetrics {
            cell_w,
            cell_h,
            ascent,
            atlas_w: atlas_w_f,
            atlas_h: atlas_h_f,
            color_atlas_w: color_atlas_w as f32,
            color_atlas_h: color_atlas_h as f32,
            gutter: layout.gutter as f32,
            padding: layout.padding as f32,
            title_h: layout.cell_title_h as f32,
        };
        let cache = &mut pane_caches[i];
        cache.shown = true;
        let fkey = frame_key(view, rect, &m, colors.id, atlas, color_atlas);
        let fp = pane_fingerprint(view, window_focused, fkey);
        let link_gen = crate::link_probe::generation();
        if !(cache.primed && cache.fingerprint == fp && cache.link_gen == link_gen) {
            stats.rebuilt += 1;
            let whole = !cache.primed || cache.frame_key != fkey;
            let (rows_done, rows) =
                lay_out(cache, whole, rect, view, window_focused, &m, font, atlas, color_atlas, colors);
            stats.rows_rebuilt += rows_done;
            stats.rows_total += rows;
            // Laying out can itself rebuild an atlas or recycle a
            // shelf; the keys recorded are the ones from before it, so
            // the next frame sees them differ and lays out again.
            cache.fingerprint = fp;
            cache.frame_key = fkey;
            cache.link_gen = link_gen;
            cache.primed = true;
        }
        if inline {
            let cache = &pane_caches[i];
            cells.extend_from_slice(&cache.cells);
            glyphs.extend_from_slice(&cache.glyphs);
            color_glyphs.extend_from_slice(&cache.color_glyphs);
        }
    }
    stats.panes_us = t_panes0.elapsed().as_micros() as u64;
    stats
}

/// Bring one pane's lists up to date; returns how many rows were laid
/// out, and how many it has.
/// One row in this many is keyed first, to tell a pane that scrolled.
const SAMPLE_EVERY: usize = 4;

/// The key of a row that was not keyed: equal to no row's key, so the
/// row counts as changed until it is keyed.
const UNKNOWN_KEY: u64 = 0;

fn lay_out(
    cache: &mut PaneInstanceCache,
    whole: bool,
    rect: &CellRect,
    view: &SessionView,
    window_focused: bool,
    m: &PaneMetrics,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    colors: crate::ui::theme::terminal::TermColors,
) -> (u32, u32) {
    PATCH.with_borrow_mut(|patch| {
        let Patch {
            cells: p_cells,
            glyphs: p_glyphs,
            color_glyphs: p_color,
            pieces,
            keys,
            row_clusters,
            old_ends,
            new_ends,
            fresh_ends,
        } = patch;
        let PaneInstanceCache { cells, glyphs, color_glyphs, head_end, rows, links, dirty, .. } = cache;
        let g = pane_geom(rect, view, window_focused, m, links, colors);
        let n_rows = g.grid.rows() as usize;
        keys.clear();
        // A pane that scrolls changes every row, every frame: a quarter
        // of its rows decides that, and the rest are not keyed at all.
        // Their keys are left unknown, so the first frame after the
        // output stops lays the pane out whole once more and keys every
        // row; the frames after it patch.
        let mut sampled_mostly_new = false;
        if !whole && rows.len() == n_rows {
            keys.resize(n_rows, UNKNOWN_KEY);
            let mut new = 0;
            for r in (0..n_rows).step_by(SAMPLE_EVERY) {
                keys[r] = row_key(&g, r as u16, row_clusters);
                new += usize::from(rows[r].key != keys[r]);
            }
            sampled_mostly_new = new * 2 > n_rows.div_ceil(SAMPLE_EVERY);
        }
        let changed = if sampled_mostly_new {
            n_rows
        } else {
            keys.clear();
            keys.extend((0..n_rows as u16).map(|r| row_key(&g, r, row_clusters)));
            if whole || rows.len() != n_rows {
                n_rows
            } else {
                rows.iter().zip(keys.iter()).filter(|(slot, key)| slot.key != **key).count()
            }
        };

        if changed * 2 > n_rows || rows.len() != n_rows {
            // most of it is new: lay it all out where it lives
            cells.clear();
            glyphs.clear();
            color_glyphs.clear();
            rows.clear();
            let mut o = PaneOut { font, atlas, color_atlas, cells, glyphs, color_glyphs };
            let lens = |o: &PaneOut| [o.cells.len() as u32, o.glyphs.len() as u32, o.color_glyphs.len() as u32];
            paint_head(rect, view, window_focused, &mut o);
            *head_end = lens(&o);
            for r in 0..n_rows as u16 {
                paint_row(&g, &mut o, r, row_clusters);
                rows.push(RowSlot { key: keys[r as usize], end: lens(&o) });
            }
            paint_tail(&g, &mut o);
            let lens = lens(&o);
            for (kind, spans) in dirty.iter_mut().enumerate() {
                spans.clear();
                spans.push((0, lens[kind]));
            }
            return (n_rows as u32, n_rows as u32);
        }

        // The head, the changed rows and the tail, laid out to one side.
        p_cells.clear();
        p_glyphs.clear();
        p_color.clear();
        pieces.clear();
        {
            let mut o = PaneOut { font, atlas, color_atlas, cells: p_cells, glyphs: p_glyphs, color_glyphs: p_color };
            let lens = |o: &PaneOut| [o.cells.len() as u32, o.glyphs.len() as u32, o.color_glyphs.len() as u32];
            paint_head(rect, view, window_focused, &mut o);
            pieces.push(Some(lens(&o)));
            for r in 0..n_rows {
                if rows[r].key == keys[r] {
                    pieces.push(None);
                } else {
                    paint_row(&g, &mut o, r as u16, row_clusters);
                    pieces.push(Some(lens(&o)));
                }
            }
            paint_tail(&g, &mut o);
            pieces.push(Some(lens(&o)));
        }

        // Then each list is rearranged around what was kept.
        let old_len = [cells.len() as u32, glyphs.len() as u32, color_glyphs.len() as u32];
        let n = n_rows + 2;
        for kind in 0..3 {
            old_ends.clear();
            new_ends.clear();
            fresh_ends.clear();
            old_ends.push(head_end[kind]);
            old_ends.extend(rows.iter().map(|s| s.end[kind]));
            old_ends.push(old_len[kind]);
            let (mut at, mut fresh_at) = (0u32, 0u32);
            for i in 0..n {
                let old_start = if i == 0 { 0 } else { old_ends[i - 1] };
                match pieces[i] {
                    Some(ends) => {
                        at += ends[kind] - fresh_at;
                        fresh_at = ends[kind];
                        fresh_ends.push(Some(ends[kind]));
                    }
                    None => {
                        at += old_ends[i] - old_start;
                        fresh_ends.push(None);
                    }
                }
                new_ends.push(at);
            }
            match kind {
                0 => relayout(cells, old_ends, new_ends, p_cells, fresh_ends, &mut dirty[0]),
                1 => relayout(glyphs, old_ends, new_ends, p_glyphs, fresh_ends, &mut dirty[1]),
                _ => relayout(color_glyphs, old_ends, new_ends, p_color, fresh_ends, &mut dirty[2]),
            }
            head_end[kind] = new_ends[0];
            for (r, slot) in rows.iter_mut().enumerate() {
                slot.end[kind] = new_ends[r + 1];
            }
        }
        for (slot, key) in rows.iter_mut().zip(keys.iter()) {
            slot.key = *key;
        }
        (changed as u32, n_rows as u32)
    })
}

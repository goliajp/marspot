//! The strip under a pane's terminal, laid out as instances.
//!
//! A strip is a grid in the pane's own cells and is drawn row by row
//! with the terminal's own painter, but it is kept apart from the
//! pane's instances: what it shows changes every second while the pane
//! may not change at all.

use std::hash::{Hash, Hasher};

use crate::font_cache::FontCache;
use crate::frame_build::session::{paint_row, PaneGeom, PaneOut};
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::{cell_grid, CellRect, Layout};
use crate::render::{SessionView, StripView};
use crate::ui::theme::terminal::TermColors;
use golia_ui_core::scene::{GlyphInstance, RectInstance as CellInstance};
use marspot_term::fast_hash::FxHasher;

/// One pane's strip as last laid out.
#[derive(Default)]
pub(crate) struct StripCache {
    /// What it was laid out from; `None` until it has been.
    key: Option<u64>,
    cells: Vec<CellInstance>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
    row_clusters: Vec<(u16, String)>,
}

fn strip_key(
    view: &SessionView,
    strip: &StripView,
    rect: &CellRect,
    layout: &Layout,
    font: &FontCache,
    colors: &TermColors,
    atlases: [&GlyphAtlas; 2],
) -> u64 {
    let mut h = FxHasher::default();
    (view.session, strip.seq, strip.top_row, strip.grid.cols(), strip.grid.rows()).hash(&mut h);
    colors.id.hash(&mut h);
    marspot_term::language::version().hash(&mut h);
    for v in [rect.x, rect.y_top, layout.cell_title_h, layout.padding, font.cell_w, font.cell_h, font.ascent] {
        v.to_bits().hash(&mut h);
    }
    for a in atlases {
        a.rebuild_count.hash(&mut h);
        a.evictions.hash(&mut h);
    }
    h.finish()
}

/// Bring every strip of the frame up to date, and append it to the
/// lists the frame draws over its panes' backgrounds.
pub(crate) fn push_strips(
    layout: &Layout,
    views: &[SessionView],
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
    cells: &mut Vec<CellInstance>,
    glyphs: &mut Vec<GlyphInstance>,
    color_glyphs: &mut Vec<GlyphInstance>,
    caches: &mut Vec<StripCache>,
) {
    for (i, (view, rect)) in views.iter().zip(&layout.cells).enumerate() {
        let Some(strip) = view.strip.as_ref() else { continue };
        if caches.len() <= i {
            caches.resize_with(i + 1, StripCache::default);
        }
        let cache = &mut caches[i];
        let colors = TermColors::current();
        let key = strip_key(view, strip, rect, layout, font, &colors, [atlas, color_atlas]);
        if cache.key != Some(key) {
            lay_out(cache, strip, rect, layout, colors, font, atlas, color_atlas);
            cache.key = Some(key);
        }
        cells.extend_from_slice(&cache.cells);
        glyphs.extend_from_slice(&cache.glyphs);
        color_glyphs.extend_from_slice(&cache.color_glyphs);
    }
}

fn lay_out(
    cache: &mut StripCache,
    strip: &StripView,
    rect: &CellRect,
    layout: &Layout,
    colors: TermColors,
    font: &mut FontCache,
    atlas: &mut GlyphAtlas,
    color_atlas: &mut GlyphAtlas,
) {
    let view = SessionView::plain(strip.grid);
    let at = cell_grid(rect, layout.cell_title_h, layout.padding, font.cell_w, font.cell_h);
    let (atlas_dims, color_dims) = (atlas.dims(), color_atlas.dims());
    let g = PaneGeom {
        view: &view,
        grid: strip.grid,
        cols: strip.grid.cols() as usize,
        cell_w: at.w as f32,
        cell_h: at.h as f32,
        ascent: font.ascent as f32,
        atlas_w: atlas_dims.0 as f32,
        atlas_h: atlas_dims.1 as f32,
        color_atlas_w: color_dims.0 as f32,
        color_atlas_h: color_dims.1 as f32,
        inner_x: at.x as f32,
        inner_y: at.row_top(strip.top_row) as f32,
        window_focused: false,
        overlay_mask: None,
        preedit_cells: Vec::new(),
        links: &[],
        colors,
    };
    let StripCache { cells, glyphs, color_glyphs, row_clusters, .. } = cache;
    cells.clear();
    glyphs.clear();
    color_glyphs.clear();
    let mut o = PaneOut { font, atlas, color_atlas, cells, glyphs, color_glyphs };
    for r in 0..strip.grid.rows() {
        paint_row(&g, &mut o, r, row_clusters);
    }
}

//! Instance building: cells and glyphs out of a grid, colour emoji
//! routing, and preedit compositions.

use super::*;
use crate::frame_build::text_run::preedit_placements;

/// The focus ring survives the neighbours' scrims.
///
/// The ring is painted in the BG pass at the seam, and the seam on
/// Where the composition lands: clusters, widths, wrapping, and
/// the bottom edge.
#[test]
fn a_composition_wraps_and_stops_at_the_last_row() {
    // Plain, from the cursor.
    assert_eq!(
        preedit_placements("abc", (2, 0), 10, 4),
        vec![(0, 2, 1, "a"), (0, 3, 1, "b"), (0, 4, 1, "c")]
    );
    // A wide cluster takes two cells and wraps whole rather than
    // being split across the edge.
    assert_eq!(
        preedit_placements("中文", (9, 0), 10, 4),
        vec![(1, 0, 2, "中"), (1, 2, 2, "文")]
    );
    // A structured composition's newline goes to the next row.
    assert_eq!(
        preedit_placements("a\nb", (0, 0), 10, 4),
        vec![(0, 0, 1, "a"), (1, 0, 1, "b")]
    );
    // Out of vertical room: what fits is placed, the rest is the
    // candidate window's job.
    assert_eq!(preedit_placements("ab", (9, 3), 10, 4), vec![(3, 9, 1, "a")]);
    assert!(preedit_placements("a", (0, 0), 0, 0).is_empty());
}

/// The cells a composition covers must be left empty by the grid
/// loops.
///
/// BG and FG are separate passes, so the preedit's opaque
/// background covers only the cells' BACKGROUND; the terminal's
/// own glyphs are drawn afterwards and land back on top of the
/// composition.  Reported against a claudecode placeholder
/// ("press up to edit queued messages") that stayed legible
/// underneath the pinyin being typed over it (2026-09-23).
#[test]
fn a_composition_hides_the_cells_it_covers() {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;

    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

    // Six columns of text under a three-cell composition typed at
    // the start of the row — the placeholder's own shape.
    let mut grid = Grid::new(10, 4);
    for (i, ch) in "abcdef".chars().enumerate() {
        grid.set_cell(i as u16, 0, Cell { ch, attrs: Default::default() });
    }
    let layout = Layout::build(
        font.cell_w * 10.0,
        font.cell_h * 4.0,
        0.0, 0.0, 0.0, 1, 1,
        font.cell_w,
        font.cell_h,
    );
    let mk = |preedit: &'static str| SessionView {
        grid: &grid,
        view_offset: 0,
        // Off, so the cursor's own glyph re-emit is not what this
        // measures.
        cursor_visible: false,
        focused: true,
        title: "",
        selection: None,
        ime_preedit: preedit,
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "", agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };
    let mut run = |v: &SessionView| -> Vec<GlyphInstance> {
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        build_instances(
            &layout, std::slice::from_ref(v), &[], 0, true,
            None, None, None, None, None, None, None,
            &mut font, &mut atlas, &mut color_atlas,
            &mut Vec::new(), &mut glyphs, &mut Vec::new(), &mut Vec::new(),
            &mut Vec::new(), &mut crate::frame_build::frame::FrameKeep::default(),
            &mut Vec::new(), &mut Vec::new(), &mut Vec::new(),
        );
        glyphs
    };
    let plain = run(&mk(""));
    let composing = run(&mk("tfg"));
    assert_eq!(plain.len(), 6, "six letters, six glyphs");
    assert_eq!(
        composing.len(),
        6,
        "three letters left + three composed: the covered cells must emit nothing, \
         or the text underneath is drawn over the composition"
    );
    // And the three that remain are the ones to the RIGHT of the
    // composition: everything at x < 3 cells belongs to it.
    let under = 3.0 * font.cell_w as f32;
    let over_composition = composing.iter().filter(|g| g.origin[0] < under).count();
    assert_eq!(over_composition, 3, "one glyph per composed cell, none from the grid");
}

#[test]
fn build_instances_emits_cells_and_glyphs() {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;

    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

    let mut grid = Grid::new(10, 4);
    let cell_a = Cell {
        ch: 'A',
        attrs: Default::default(),
    };
    let cell_b = Cell {
        ch: 'B',
        attrs: Default::default(),
    };
    grid.set_cell(0, 0, cell_a);
    grid.set_cell(1, 0, cell_b);

    let layout = Layout::build(
        font.cell_w * 10.0,
        font.cell_h * 4.0,
        0.0,
        0.0,
        0.0,
        1,
        1,
        font.cell_w,
        font.cell_h,
    );
    let view = SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: true,
        focused: true,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "", agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };

    let mut cells: Vec<CellInstance> = Vec::new();
    let mut glyphs: Vec<GlyphInstance> = Vec::new();
    let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
    let mut dots: Vec<CellInstance> = Vec::new();
    build_instances(
        &layout,
        std::slice::from_ref(&view),
        &[],
        0,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        &mut font,
        &mut atlas,
        &mut color_atlas,
        &mut cells,
        &mut glyphs,
        &mut color_glyphs,
        &mut dots,
        &mut Vec::new(),
        &mut crate::frame_build::frame::FrameKeep::default(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
    );

    // Expected glyph instances:
    //  - 'B' at col 1 (FG colour) — the cursor cell at col 0 is
    //    skipped during the FG emit because it's solid-cursor
    //  - 'A' at col 0 (BG colour) re-rendered after the cursor block
    //  Order in vec is FG-pass-first then cursor re-render, so [B, A].
    //  cells: at minimum a terminal-bg fill + cursor block.
    //  Focus outline (×4) only fires when gutter > 0, which a
    //  1×1 layout doesn't have, so it can be absent here.
    assert!(cells.len() >= 2, "got cells.len()={}", cells.len());
    assert_eq!(glyphs.len(), 2, "got glyphs.len()={}", glyphs.len());
    // Whichever order they came out in, the two glyphs are exactly
    // one cell apart in x (modulo CT bearing differences ≤2 px).
    let dx = (glyphs[1].origin[0] - glyphs[0].origin[0]).abs();
    let cw = font.cell_w as f32;
    assert!(
        (dx - cw).abs() < 3.0,
        "glyphs should be ~one cell apart, got |dx|={dx} vs cell_w={cw}"
    );
}

/// A colour emoji must route to the COLOUR glyph buffer + colour atlas,
/// not the mono one — the whole point of the colour-emoji path.
#[test]
fn build_instances_routes_color_emoji() {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;

    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 512, 512, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 512, 512, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

    // Skip on the (macOS-impossible) chance there's no colour emoji
    // font — the routing decision keys off the resolved font's
    // colour-glyphs trait, so without one there's nothing to assert.
    let (emoji_font_idx, emoji_glyph) = font.resolve_char('😀', false, false);
    if emoji_glyph == 0 || !font.is_color_font(emoji_font_idx) {
        return;
    }

    let mut grid = Grid::new(10, 4);
    grid.set_cell(0, 0, Cell { ch: '😀', attrs: Default::default() });
    grid.set_cell(2, 0, Cell { ch: 'A', attrs: Default::default() });

    let layout = Layout::build(
        font.cell_w * 10.0,
        font.cell_h * 4.0,
        0.0,
        0.0,
        0.0,
        1,
        1,
        font.cell_w,
        font.cell_h,
    );
    let view = SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: false, // render every cell, don't skip under cursor
        focused: true,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "", agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };

    let mut cells: Vec<CellInstance> = Vec::new();
    let mut glyphs: Vec<GlyphInstance> = Vec::new();
    let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
    let mut dots: Vec<CellInstance> = Vec::new();
    build_instances(
        &layout,
        std::slice::from_ref(&view),
        &[],
        0,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        &mut font,
        &mut atlas,
        &mut color_atlas,
        &mut cells,
        &mut glyphs,
        &mut color_glyphs,
        &mut dots,
        &mut Vec::new(),
        &mut crate::frame_build::frame::FrameKeep::default(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
    );

    assert_eq!(color_glyphs.len(), 1, "emoji should emit one colour glyph");
    assert_eq!(glyphs.len(), 1, "the 'A' should be the only mono glyph");
    assert!(color_atlas.cache_len() >= 1, "colour atlas should hold the emoji");
}

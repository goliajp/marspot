//! Fixed tool rows and highlight spans: where grid glyphs land when a
//! tool pins rows, and how highlights map to cells.

use super::*;

// ─── C1: PaneTool framework layout-shrink tests ───────────────

/// Build instances for a 2-row grid with `top_fixed_h_cells = N`.
/// Returns the glyphs vec for inspection.  Helper shared by the
/// two layout-shrink tests below.
fn build_glyphs_with_top_fixed(top_fixed: u16) -> Vec<GlyphInstance> {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;
    let device = system_default_device().expect("metal device");
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let mut grid = Grid::new(10, 4);
    grid.set_cell(0, 0, Cell { ch: 'A', attrs: Default::default() });
    grid.set_cell(2, 0, Cell { ch: 'B', attrs: Default::default() });
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
        cursor_visible: false,
        focused: true,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "", agent_tui: false, cwd: "",
        top_fixed_h_cells: top_fixed,
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
    glyphs
}

/// C1 — a `TopFixed` tool claiming N rows shifts the grid's
/// glyph origin Y down by exactly `N * cell_h`.  No tool, no
/// shift (byte-identical to pre-C1, per §12.C1 DoD).
#[test]
fn c1_top_fixed_tool_shifts_grid_glyph_y() {
    if system_default_device().is_err() {
        return;
    }
    let font_cell_h = FontCache::build().expect("font").cell_h as f32;
    let g0 = build_glyphs_with_top_fixed(0);
    let g2 = build_glyphs_with_top_fixed(2);
    assert!(!g0.is_empty(), "baseline produced glyphs");
    assert_eq!(g0.len(), g2.len(), "glyph count must be identical");
    // Match by uv (identifies the character; A vs B have distinct
    // uv).  For each glyph, the v2 origin_y should be exactly
    // 2 * cell_h above (numerically higher, i.e. lower on screen).
    for (g0_inst, g2_inst) in g0.iter().zip(g2.iter()) {
        assert_eq!(g0_inst.uv0, g2_inst.uv0, "uv must match (same char)");
        let dy = g2_inst.origin[1] - g0_inst.origin[1];
        let expected = 2.0 * font_cell_h.round();
        assert!(
            (dy - expected).abs() < 0.5,
            "TopFixed=2 should shift glyph Y by ~{expected}; got dy={dy}"
        );
    }
}

/// C4 — `highlight_spans` covering two viewport rows emits 2
/// `CellInstance` fills tinted `HIGHLIGHT_BG`, one per row at
/// the listed column range.  Verifies span row-filter +
/// per-row col clipping + bounded cells.len() growth (≤ 1
/// `CellInstance` per spanned row).
#[test]
fn c4_highlight_two_row_span_emits_two_cells() {
    use crate::grid::Grid;
    use crate::layout::Layout;
    if system_default_device().is_err() {
        return;
    }
    let device = system_default_device().expect("metal device");
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let grid = Grid::new(10, 4);
    let layout = Layout::build(
        font.cell_w * 10.0,
        font.cell_h * 4.0,
        0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h,
    );
    // Two-row span: row 1 col 5-9 (5 cols), row 2 col 0-3 (4 cols).
    let spans = vec![
        HighlightSpan { view_row: 1, col_start: 5, col_end_inclusive: 9 },
        HighlightSpan { view_row: 2, col_start: 0, col_end_inclusive: 3 },
    ];
    let view = SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: false,
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
        highlight_spans: &spans,
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };
    // Baseline cell count (no highlight) for the same layout/grid.
    let baseline_view = SessionView {
        grid: view.grid,
        view_offset: view.view_offset,
        cursor_visible: view.cursor_visible,
        focused: view.focused,
        title: view.title,
        selection: view.selection,
        ime_preedit: view.ime_preedit,
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: view.right_badge, agent_tui: view.agent_tui,
        cwd: "",
        top_fixed_h_cells: view.top_fixed_h_cells,
        bot_fixed_h_cells: view.bot_fixed_h_cells,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };
    let mut run = |v: &SessionView| -> Vec<CellInstance> {
        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        let mut dots: Vec<CellInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(v),
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
        cells
    };
    let baseline = run(&baseline_view);
    let with_hl = run(&view);
    let extra = with_hl.len() - baseline.len();
    assert_eq!(
        extra, 2,
        "expected exactly 2 extra CellInstance for two-row span; got {extra}"
    );
    // Find the highlight cells (colour = HIGHLIGHT_BG).
    let highlight: Vec<&CellInstance> = with_hl
        .iter()
        // Eight bits now, so the colour is matched as the byte it
        // becomes rather than as the float it was written as.
        .filter(|c| c.color == crate::ui::theme::terminal::TermColors::current().search_hit)
        .collect();
    assert_eq!(highlight.len(), 2);
    // Each highlight cell sits one cell_h below the previous (consecutive rows).
    // `push_session` rounds cell_w / cell_h to integer pixels before
    // emitting cell instances; mirror the rounding in our assertions.
    let cell_h = (font.cell_h as f32).round();
    let mut ys: Vec<f32> = highlight.iter().map(|c| c.origin[1]).collect();
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!((ys[1] - ys[0] - cell_h).abs() < 1.0, "row gap should be cell_h");
    // Sizes: 5 cells wide vs 4 cells wide.
    let cell_w = (font.cell_w as f32).round();
    let widths: Vec<f32> = highlight.iter().map(|c| c.size[0]).collect();
    assert!(
        widths.iter().any(|w| (w - 5.0 * cell_w).abs() < 1.0),
        "expected a 5-cell-wide highlight (cell_w={cell_w}); got widths {widths:?}"
    );
    assert!(
        widths.iter().any(|w| (w - 4.0 * cell_w).abs() < 1.0),
        "expected a 4-cell-wide highlight (cell_w={cell_w}); got widths {widths:?}"
    );
}

/// C4 — a highlight span whose `view_row` is past the grid's
/// row count is silently skipped (no panic, no emission).
#[test]
fn c4_highlight_out_of_range_row_is_skipped() {
    use crate::grid::Grid;
    use crate::layout::Layout;
    if system_default_device().is_err() {
        return;
    }
    let device = system_default_device().expect("metal device");
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let grid = Grid::new(10, 4);
    let layout = Layout::build(
        font.cell_w * 10.0,
        font.cell_h * 4.0,
        0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h,
    );
    // view_row = 9 is past grid.rows() = 4 — should be skipped.
    let spans = vec![
        HighlightSpan { view_row: 9, col_start: 0, col_end_inclusive: 4 },
    ];
    let view = SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: false,
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
        highlight_spans: &spans,
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
    // No HIGHLIGHT_BG cell should be present.
    let highlight_count = cells
        .iter()
        // Eight bits now, so the colour is matched as the byte it
        // becomes rather than as the float it was written as.
        .filter(|c| c.color == crate::ui::theme::terminal::TermColors::current().search_hit)
        .count();
    assert_eq!(highlight_count, 0);
}

/// C1 — a `BottomFixed` tool DOES NOT shift the grid's glyph
/// origin (only TopFixed does).  This locks down the invariant
/// that the BottomFixed slot reserves space against the pane's
/// lower edge — the grid sits where it always did.
#[test]
fn c1_bottom_fixed_tool_does_not_shift_grid_glyph_y() {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;
    if system_default_device().is_err() {
        return;
    }
    let device = system_default_device().expect("metal device");
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let mut grid = Grid::new(10, 4);
    grid.set_cell(0, 0, Cell { ch: 'A', attrs: Default::default() });
    let layout = Layout::build(
        font.cell_w * 10.0,
        font.cell_h * 4.0,
        0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h,
    );
    let view_base = SessionView {
        grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
        title: "", selection: None, ime_preedit: "", dormant: false,
                                                                            recede: 0,
            scrim: 0.0,
        right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };
    let view_bot = SessionView {
        grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
        title: "", selection: None, ime_preedit: "", dormant: false,
                                                                            recede: 0,
            scrim: 0.0,
        right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 2,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };
    let mut run = |view: &SessionView| -> Vec<GlyphInstance> {
        let mut cells: Vec<CellInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();
        let mut color_glyphs: Vec<GlyphInstance> = Vec::new();
        let mut dots: Vec<CellInstance> = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(view),
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
        glyphs
    };
    let g0 = run(&view_base);
    let gb = run(&view_bot);
    assert_eq!(g0.len(), gb.len());
    for (a, b) in g0.iter().zip(gb.iter()) {
        assert_eq!(a.origin[1], b.origin[1], "bot_fixed must NOT shift grid Y");
    }
}

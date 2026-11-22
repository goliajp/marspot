//! Snapshots of whole scenes: a terminal frame and the window chrome
//! decorations.

use super::font_snapshot_tests::snapshot_mode;
use super::*;
use crate::snapshot_kit::{assert_snapshot_ssim, write_snapshot_png, FONT_SNAPSHOT_DIR as SNAPSHOT_DIR};

/// Phase 9 (extended) — terminal scene composite.  Approximates
/// the production PTY render path through the sheet:  cell BG
/// rect runs for the selection band, a brighter rect for the
/// cursor block, plus multi-line mono Monaco text on top.  Tests
/// the same primitive composition (`Rect` BG → `Text` FG) the
/// real PTY frame uses, just driven by hand instead of from a
/// live `Grid`.  Real `Grid` → `render_layout_to_texture` readback
/// needs a `StorageModeShared` target texture that the renderer
/// doesn't currently expose;  threading that through is a
/// separate refactor.  This sheet approximation locks the
/// visual composition contract:  no overflow between cells, no
/// glyph bleed across the selection edge, BG layering preserved.
#[test]
fn font_v5_terminal_scene_snapshot() {
    if snapshot_mode().is_none() {
        return;
    }
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let w_px: u32 = 1200;
    let h_px: u32 = 600;
    let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
    // Each "cell" in the sheet-pt space is half the phys cell
    // (the sheet is drawn at 2×).  Use Monaco cell width via
    // chrome_font_metrics so the simulated grid sits on the same
    // pitch as the real terminal.
    let cell_w_pt = (chrome_cell_w as f64) / 2.0;
    let cell_h_pt = (chrome_cell_h as f64) / 2.0;
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    let mut sheet = Sheet::new(2.0);
    let pad_x_pt = 16.0;
    let pad_y_pt = 16.0;
    // Page BG — production terminal dark.
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    // Simulated terminal lines — log-style output with prompts.
    let lines = [
        "$ git status",
        "On branch develop",
        "Your branch is up to date with 'origin/develop'.",
        "",
        "Changes not staged for commit:",
        "  (use \"git add <file>...\" to update what will be committed)",
        "  (use \"git restore <file>...\" to discard changes)",
        "",
        "    modified:   src/render_metal.rs",
        "    modified:   bench/font-rendering/snapshots/...",
        "",
        "$ cargo nextest run --lib",
        "    Finished `test` profile [unoptimized + debuginfo]",
        "        PASS [  0.018s] (1/2) marspot tests::...",
        "        PASS [  0.018s] (2/2) marspot tests::...",
    ];
    // Selection band — rows 3..5 (0-indexed), cols 4..50.
    // Match iTerm2 default selection blue (sub-1.0 alpha so the
    // cell BG underneath still shows through faintly).
    let sel_color = Color::rgba(46, 92, 158, 0.78);
    for row in 3..=5usize {
        let y = pad_y_pt + (row as f64) * cell_h_pt;
        let (col_start, col_end) = if row == 3 {
            (4.0, lines.get(row).map(|l| l.len() as f64).unwrap_or(0.0))
        } else if row == 5 {
            (0.0, 30.0)
        } else {
            (0.0, lines.get(row).map(|l| l.len() as f64).unwrap_or(0.0))
        };
        let w = (col_end - col_start) * cell_w_pt;
        if w > 0.0 {
            sheet
                .rect()
                .at(pad_x_pt + col_start * cell_w_pt, y)
                .size(w, cell_h_pt)
                .fill(sel_color)
                .draw();
        }
    }
    // Cursor cell — block at end of last log line (focused/live).
    let cursor_row = lines.len() - 1;
    let cursor_col = lines[cursor_row].len();
    // The colour is the palette's cursor because `OSC 12 ; ? ST`
    // asks for it; the alpha stays here, because how solid the
    // block looks is a drawing decision and not part of the
    // answer.
    let (cr, cg, cb) = marspot_term::palette::current().cursor;
    let cursor_color = Color::rgba(cr, cg, cb, 0.90);
    sheet
        .rect()
        .at(
            pad_x_pt + (cursor_col as f64) * cell_w_pt,
            pad_y_pt + (cursor_row as f64) * cell_h_pt,
        )
        .size(cell_w_pt, cell_h_pt)
        .fill(cursor_color)
        .draw();
    // Text layer last so glyphs ride on top of BG rects.
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (i, line) in lines.iter().enumerate() {
        let y = pad_y_pt + (i as f64) * cell_h_pt;
        sheet
            .text(pad_x_pt, y, line)
            .color(fg)
            .draw();
    }
    let bytes = renderer
        .render_sheet_to_bitmap(
            w_px,
            h_px,
            &sheet,
            chrome_cell_w,
            chrome_cell_h,
            chrome_ascent,
            false, // Mono path
        )
        .expect("sheet render");

    let mut rgba = vec![0u8; bytes.len()];
    for i in (0..bytes.len()).step_by(4) {
        rgba[i] = bytes[i + 2];
        rgba[i + 1] = bytes[i + 1];
        rgba[i + 2] = bytes[i];
        rgba[i + 3] = bytes[i + 3];
    }

    let out_dir = std::path::PathBuf::from(SNAPSHOT_DIR);
    std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
    let out_path = out_dir.join("font_v5_terminal_scene.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 terminal scene] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

/// Phase 9 (extended) — chrome decoration composite (rounded
/// rect + shadow + border).  F3+1.7 fixed the process panel
/// modal's BG shadow + border path; lock the rendering so a
/// regression(shadow blur clamped / radius drift / border
/// over-paint)surfaces on the next snapshot run instead of
/// being noticed weeks later as "the panel chrome looks off".
///
/// 4 columns × 1 row showcase:
///   1.  Solid rounded rect — minimal modal frame
///   2.  + Shadow blur 12 / offset (0, 4) — F3+1.7 modal
///   3.  + Hairline border on top of shadow — a card
///   4.  Stacked depth — three layered rects with progressive
///       shadow blur (modal-on-modal, popover-on-popover)
#[test]
fn font_v5_chrome_decoration_snapshot() {
    if snapshot_mode().is_none() {
        return;
    }
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let w_px: u32 = 1400;
    let h_px: u32 = 500;
    let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    let mut sheet = Sheet::new(2.0);
    // Page BG — neutral mid so shadows show.
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(28, 30, 36, 1.0))
        .draw();
    let panel_fill = Color::rgba(35, 38, 46, 1.0);
    let border_color = Color::rgba(80, 86, 100, 1.0);
    let shadow_color = Color::rgba(0, 0, 0, 0.55);
    let col_w = 140.0;
    let col_h = 100.0;
    let col_gap = 30.0;
    let top_y = 40.0;
    let label_y = 170.0;
    let label_color = Color::rgba(170, 178, 195, 1.0);
    // Col 1 — solid rounded rect.
    sheet
        .rect()
        .at(40.0, top_y)
        .size(col_w, col_h)
        .fill(panel_fill)
        .radius(10.0)
        .draw();
    sheet
        .text(40.0, label_y, "1. radius")
        .color(label_color)
        .draw();
    // Col 2 — rounded rect with shadow blur.
    let col2_x = 40.0 + col_w + col_gap;
    sheet
        .rect()
        .at(col2_x, top_y)
        .size(col_w, col_h)
        .fill(panel_fill)
        .radius(10.0)
        .shadow(12.0, shadow_color)
        .draw();
    sheet
        .text(col2_x, label_y, "2. + shadow")
        .color(label_color)
        .draw();
    // Col 3 — rounded rect with shadow AND border (F3+1.7 modal).
    let col3_x = col2_x + col_w + col_gap;
    sheet
        .rect()
        .at(col3_x, top_y)
        .size(col_w, col_h)
        .fill(panel_fill)
        .radius(10.0)
        .border(1.0, border_color)
        .shadow(12.0, shadow_color)
        .draw();
    sheet
        .text(col3_x, label_y, "3. + border")
        .color(label_color)
        .draw();
    // Col 4 — stacked depth (3 rects layered with growing shadow).
    let col4_x = col3_x + col_w + col_gap;
    // Back layer — biggest shadow blur,offset down.
    sheet
        .rect()
        .at(col4_x + 20.0, top_y)
        .size(col_w - 20.0, col_h - 30.0)
        .fill(Color::rgba(25, 28, 35, 1.0))
        .radius(8.0)
        .shadow(20.0, shadow_color)
        .draw();
    // Middle layer.
    sheet
        .rect()
        .at(col4_x + 12.0, top_y + 12.0)
        .size(col_w - 20.0, col_h - 30.0)
        .fill(Color::rgba(30, 33, 41, 1.0))
        .radius(8.0)
        .shadow(14.0, shadow_color)
        .draw();
    // Top layer.
    sheet
        .rect()
        .at(col4_x + 4.0, top_y + 24.0)
        .size(col_w - 20.0, col_h - 30.0)
        .fill(Color::rgba(38, 42, 52, 1.0))
        .radius(8.0)
        .border(1.0, border_color)
        .shadow(8.0, shadow_color)
        .draw();
    sheet
        .text(col4_x, label_y, "4. depth stack")
        .color(label_color)
        .draw();
    let bytes = renderer
        .render_sheet_to_bitmap(
            w_px,
            h_px,
            &sheet,
            chrome_cell_w,
            chrome_cell_h,
            chrome_ascent,
            false,
        )
        .expect("sheet render");

    let mut rgba = vec![0u8; bytes.len()];
    for i in (0..bytes.len()).step_by(4) {
        rgba[i] = bytes[i + 2];
        rgba[i + 1] = bytes[i + 1];
        rgba[i + 2] = bytes[i];
        rgba[i + 3] = bytes[i + 3];
    }

    let out_dir = std::path::PathBuf::from(SNAPSHOT_DIR);
    std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
    let out_path = out_dir.join("font_v5_chrome_decoration.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 chrome decoration] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

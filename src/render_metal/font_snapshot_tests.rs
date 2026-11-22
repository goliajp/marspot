//! Font snapshots: the mono grid, box drawing, subpixel positions and
//! small chrome sizes, each against a frozen picture.

use super::*;
use crate::snapshot_kit::{assert_snapshot_ssim, write_snapshot_png, FONT_SNAPSHOT_DIR as SNAPSHOT_DIR};

pub(super) fn snapshot_mode() -> Option<crate::snapshot_kit::SnapshotMode> {
    crate::snapshot_kit::snapshot_mode_in(std::path::Path::new(SNAPSHOT_DIR))
}

/// Phase 9 (extended) — visual-regression strings.  Renders the
/// canonical Mono Monaco workload (ASCII + CJK cascade + emoji
/// + kerning/ligature lines) as a single PNG so a reviewer can
///
/// eyeball it AND a future SSIM gate can diff it byte-for-byte
/// against a frozen baseline (see `snapshot_mode` for what
/// `MARSPOT_FONT_SNAPSHOT` changes).  Output:
///   bench/font-rendering/snapshots/font_v5_mono_grid.png
///
/// Workflow:
///   MARSPOT_FONT_SNAPSHOT=1 cargo nextest run -p marspot --lib \
///     font_v5_mono_grid_snapshot
#[test]
fn font_v5_mono_grid_snapshot() {
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
    // Five lines, one per font-feature axis the design doc §15
    // pins as a v5 acceptance test:
    //   L1 ASCII alphabet  — Monaco cell-pitch sanity
    //   L2 CJK cascade     — fallback chain into PingFang/Hiragino/AppleSDGothic
    //   L3 emoji + ZWJ     — colour atlas + ZWJ cluster
    //   L4 kerning/ligature — `Ta` / `fi` / `==>` shaping in mono context
    //   L5 mixed RTL hello — bidi placeholder (RTL routing comes later)
    let lines = [
        "The quick brown fox jumps over the lazy dog 0123456789",
        "你好世界  こんにちは  안녕하세요",
        "👨‍👩‍👧‍👦  🍕  🇯🇵  🌈  ⭐  ❤️",
        "Tax  fi  fl  ff  ==>  !=  !==",
        "Bidi placeholder: Hello مرحبا עברית",
    ];
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    let mut sheet = Sheet::new(2.0);
    let line_h_pt = (chrome_cell_h as f64 * 1.6) / 2.0;
    let pad_x_pt = 20.0;
    let pad_y_pt = 16.0;
    // Solid dark BG matches the production terminal BG so the
    // glyphs read at a glance.
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (i, line) in lines.iter().enumerate() {
        let y_pt = pad_y_pt + (i as f64) * line_h_pt;
        sheet
            .text(pad_x_pt, y_pt, line)
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
            false, // Mono path (Monaco)
        )
        .expect("sheet render");

    // BGRA → RGBA channel swap for PNG.
    let mut rgba = vec![0u8; bytes.len()];
    for i in (0..bytes.len()).step_by(4) {
        rgba[i] = bytes[i + 2];
        rgba[i + 1] = bytes[i + 1];
        rgba[i + 2] = bytes[i];
        rgba[i + 3] = bytes[i + 3];
    }

    let out_dir = std::path::PathBuf::from(SNAPSHOT_DIR);
    std::fs::create_dir_all(&out_dir).expect("mkdir snapshots");
    let out_path = out_dir.join("font_v5_mono_grid.png");
    // Phase 9 — SSIM gate (see assert_snapshot_ssim doc).
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 mono grid] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

/// Phase 9 (extended) — box-drawing + block-element pieces.
/// Validates the per-codepoint custom raster (`box_drawing_arms`
/// + `block_element_rects` in render_metal.rs) — the path that
///
/// can fall out of sync with `cell_h` / baseline if any of the
/// chrome-font metrics drift.  Locks the rendered pixels so a
/// hairline gap at any `┌─┐│└─┘` junction surfaces as an SSIM
/// drop instead of being noticed by the user months later.
///
/// Same env triplet as the other snapshots (`MARSPOT_FONT_SNAPSHOT`):
/// =0 → skip, =1 → write PNG fixture, unset or =check → SSIM ≥ 0.98
/// vs the committed baseline.  Bypassed by SF Pro / variable
/// weight (those are covered by `font_v5_showcase_snapshot`);
/// this one is purely the Monaco custom-raster path.
#[test]
fn font_v5_box_drawing_snapshot() {
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
    let w_px: u32 = 1000;
    let h_px: u32 = 500;
    let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
    // Five lines, each a tight stress on the custom raster path:
    //   L1 — light frame + horizontal pieces;  contiguous corners
    //        must line up cell-by-cell.
    //   L2 — heavy frame + cross + tee in the standard cluster.
    //   L3 — double-line frame (`╔═╗`) for the variant raster.
    //   L4 — block elements (full / half / quarter / shade).
    //   L5 — mixed light/heavy junctions, the case that broke
    //        most often in 2025-Q4 raster regressions.
    let lines = [
        "┌─────┬─────┐  ╭─────╮  ┏━━━━━┓",
        "│ AAA │ BBB │  │ CCC │  ┃ DDD ┃",
        "└─────┴─────┘  ╰─────╯  ┗━━━━━┛",
        "█▓▒░  ▀▄  ▌▐  ▔▁  ▍▎▏  ▕",
        "├─┼─┤ ╠═╬═╣ ┝━┿━┥ ┠─╂─┨",
    ];
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    let mut sheet = Sheet::new(2.0);
    let line_h_pt = (chrome_cell_h as f64 * 1.6) / 2.0;
    let pad_x_pt = 20.0;
    let pad_y_pt = 16.0;
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (i, line) in lines.iter().enumerate() {
        let y_pt = pad_y_pt + (i as f64) * line_h_pt;
        sheet
            .text(pad_x_pt, y_pt, line)
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
    let out_path = out_dir.join("font_v5_box_drawing.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 box drawing] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

/// Phase 9 (extended) — sub-pixel positioning fingerprint.
/// Phase 4 quantises every CTLine pen-x to one of 4 buckets
/// (`subpx_x ∈ 0..4`) so the atlas can reuse a single bitmap
/// across pixel-aligned and 0.25/0.5/0.75-shifted positions.
/// A regression on either side of that quantisation (bucket
/// miscalc → wrong slot, or atlas slot wrong-shifted) shows up
/// as character bleed on dense glyph runs:  `iiii` smears,
/// `lll` blends, `AVAV` kerning floats.  Lock the rendered
/// fingerprint here so the regression surfaces as an SSIM drop
/// at `bin/font-snapshot-check.sh` instead of a user-reported
/// "fonts look slightly off" two months later.
///
/// Mono Monaco only — chrome SF Pro path's subpx fingerprint is
/// covered as a row inside `font_v5_showcase_snapshot`.
#[test]
fn font_v5_subpx_fingerprint_snapshot() {
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
    let w_px: u32 = 1000;
    let h_px: u32 = 500;
    let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
    // Each line stresses a different sub-pixel scenario:
    //   L1 — dense vertical-stem runs (`iiii lll`): atlas slot
    //        x-shift errors show up as inter-glyph bleed.
    //   L2 — wide-kerned letter pairs that ride sub-pixel
    //        boundaries (`AV WA Yo Ta`).
    //   L3 — narrow latin + punctuation (`!?,.;:`) — typical
    //        dense punctuation in code / log output.
    //   L4 — same letters repeated at varying counts to exercise
    //        every bucket cell-by-cell (`a aa aaa aaaa`).
    //   L5 — mixed slot widths via CJK fullwidth (`你好` ≡ 2
    //        cells each), pinning bucket transitions across the
    //        cluster boundary.
    let lines = [
        "iiiiii  lllll  IIIIII  !!!!!!",
        "AVAVAV  WAWA  YoYoYo  TaTa  fifi",
        "Code: foo(bar, baz);  !?,.;:  /* x */",
        "a aa aaa aaaa aaaaa aaaaaa aaaaaaa",
        "你好 世界 こんにちは 안녕 a b c",
    ];
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    let mut sheet = Sheet::new(2.0);
    let line_h_pt = (chrome_cell_h as f64 * 1.6) / 2.0;
    let pad_x_pt = 20.0;
    let pad_y_pt = 16.0;
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (i, line) in lines.iter().enumerate() {
        let y_pt = pad_y_pt + (i as f64) * line_h_pt;
        sheet
            .text(pad_x_pt, y_pt, line)
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
    let out_path = out_dir.join("font_v5_subpx_fingerprint.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 subpx fingerprint] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

/// Phase 9 (extended) — chrome SF Pro at small point sizes.
/// `font_v5_showcase_snapshot` already covers the SF Pro path
/// at production sizes (TITLE=22 / H=15 / BODY=13 / EMOJI=20),
/// but the riskiest regressions in chrome rendering hit small
/// type:  11–13 pt is where sub-pixel AA, hinting, and the
/// Monaco fallback for tiny ASCII all sit on knife edges.  Tab
/// strips, sidebar labels, status badges all live here, and a
/// silent drift surfaces as "fonts feel off" weeks after the
/// fact — exactly the regression class an SSIM gate catches
/// for free.
///
/// One column per size (11 / 12 / 13 / 14 pt), each rendering
/// the same ASCII + CJK + emoji line so the eye can scan
/// vertically for cell-pitch / x-height drift.
#[test]
fn font_v5_chrome_small_sizes_snapshot() {
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
    // For each row we render at a different sub-13-pt size via
    // `Text::ui(size_pt, weight, opts)` so the SF Pro variable-
    // weight pipeline + CTLine shape + GlyphAtlas natural-bbox
    // raster all line up against the size_q bucket.  Sample
    // string mixes ASCII + CJK + ligature pair so each row
    // probes a different worry per size:
    let lines: &[(f64, &str)] = &[
        (11.0, "11pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
        (12.0, "12pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
        (13.0, "13pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
        (14.0, "14pt  The quick brown fox jumps  你好  fi fl  ⌘C"),
    ];
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    use crate::font_shape::ShapeOptions;
    let mut sheet = Sheet::new(2.0);
    let pad_x_pt = 20.0;
    let pad_y_pt = 16.0;
    let line_gap_pt = 28.0;
    let opts = ShapeOptions::full();
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (i, (pt, line)) in lines.iter().enumerate() {
        let y_pt = pad_y_pt + (i as f64) * line_gap_pt;
        let size_q = crate::glyph_atlas::GlyphKey::size_q_for(*pt);
        sheet
            .text(pad_x_pt, y_pt, line)
            .color(fg)
            .ui()
            .ui_size_q(size_q)
            .weight(400)
            .opts(opts)
            .draw();
    }
    // Trailing block: 100/400/700/900 weight stack at 12pt so
    // variable-weight drift (one variant slipped out of cache)
    // also shows up here, not just in the big showcase.
    let weight_rows: &[(u16, &str)] = &[
        (100, "12pt w100  The quick brown fox jumps"),
        (400, "12pt w400  The quick brown fox jumps"),
        (700, "12pt w700  The quick brown fox jumps"),
        (900, "12pt w900  The quick brown fox jumps"),
    ];
    let weight_size_q = crate::glyph_atlas::GlyphKey::size_q_for(12.0);
    let weight_pad_y_pt = pad_y_pt + (lines.len() as f64) * line_gap_pt + 16.0;
    for (i, (w, line)) in weight_rows.iter().enumerate() {
        let y_pt = weight_pad_y_pt + (i as f64) * 22.0;
        sheet
            .text(pad_x_pt, y_pt, line)
            .color(fg)
            .ui()
            .ui_size_q(weight_size_q)
            .weight(*w)
            .opts(opts)
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
            true, // ui_font=true — emit SF Pro path
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
    let out_path = out_dir.join("font_v5_chrome_small_sizes.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 chrome small sizes] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

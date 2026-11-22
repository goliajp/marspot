//! Font snapshots for fallback and shaping: CJK baselines, colour
//! emoji and OpenType options.

use super::font_snapshot_tests::snapshot_mode;
use super::*;
use crate::snapshot_kit::{assert_snapshot_ssim, write_snapshot_png, FONT_SNAPSHOT_DIR as SNAPSHOT_DIR};

/// Phase 9 (extended) — chrome CJK fallback baseline alignment.
/// SF Pro doesn't ship CJK glyphs;  CoreText falls back through
/// `PingFang SC` (Simplified Chinese) → `Hiragino Sans` /
/// `Hiragino Mincho` (Japanese) → `Apple SD Gothic Neo` (Korean)
/// per-cluster.  When per-glyph baseline / ascent differ across
/// fonts, mixed-script lines drift — Latin + CJK on the same row
/// no longer share the same writing baseline, and the eye reads
/// it as "this looks wonky".  At chrome small sizes (11-13pt) the
/// drift is sub-pixel and easy to miss;  at large sizes the drift
/// scales linearly with size.  Lock the alignment fingerprint at
/// 24pt + 36pt so any future fallback-chain reorder or font
/// metrics re-sync surfaces as an SSIM hit.
#[test]
fn font_v5_cjk_fallback_baseline_snapshot() {
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
    let h_px: u32 = 600;
    let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
    // Each row mixes Latin (SF Pro) with the three CJK regions
    // back-to-back.  Vertical scan tells you whether the four
    // baselines stay aligned across the size + weight combo.
    // 36pt locks coarse drift;  24pt + 18pt covers the rest of
    // the body-text range.
    let rows: &[(f64, u16, &str)] = &[
        (36.0, 600, "Hello 你好 こんにちは 안녕"),
        (24.0, 400, "Hello 你好 こんにちは 안녕 — mixed baseline"),
        (24.0, 700, "Bold 你好 こんにちは 안녕 — bold mix"),
        (18.0, 400, "18pt mixed 中文 + English + 日本語 + 한국어"),
        (18.0, 400, "Mac 偏好设定 · Mac の環境設定 · Mac 환경설정"),
    ];
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    use crate::font_shape::ShapeOptions;
    let mut sheet = Sheet::new(2.0);
    let pad_x_pt = 20.0;
    // First row is 36 pt — ascent eats ~30 pt above the baseline,
    // so start the cursor ~36 pt down or the top of "Hello" gets
    // clipped at the image edge.
    let mut y_pt = 36.0;
    let opts = ShapeOptions::full();
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (pt, weight, line) in rows.iter() {
        let size_q = crate::glyph_atlas::GlyphKey::size_q_for(*pt);
        sheet
            .text(pad_x_pt, y_pt, line)
            .color(fg)
            .ui()
            .ui_size_q(size_q)
            .weight(*weight)
            .opts(opts)
            .draw();
        // Row gap proportional to size so 36pt + 24pt + 18pt
        // pack into 600 px without overlap.
        y_pt += pt * 1.4;
    }
    let bytes = renderer
        .render_sheet_to_bitmap(
            w_px,
            h_px,
            &sheet,
            chrome_cell_w,
            chrome_cell_h,
            chrome_ascent,
            true, // ui_font=true — SF Pro path
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
    let out_path = out_dir.join("font_v5_cjk_fallback_baseline.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 cjk fallback baseline] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

/// Phase 9 (extended) — emoji ZWJ + color cluster fingerprint.
/// Phase 7 promises chrome color-emoji (BGRA atlas + dedicated
/// `fg_color_pipeline`).  The color path is independent of the
/// alpha mono path — a regression here usually surfaces as
/// "emoji come out gray silhouettes" (the pre-Phase-7 fall-back
/// when the colour atlas wasn't wired).  Lock the colour bytes
/// so any silent fall-off back to silhouette / wrong glyph
/// substitution / ZWJ cluster split surfaces as an SSIM drop.
///
/// Family ZWJ (`👨‍👩‍👧‍👦`) is the canonical ZWJ-cluster stress
/// — drop a single ZWJ join and it explodes into 4 separate
/// glyphs.  Flag (`🇯🇵` `🇰🇷` `🇺🇸` `🇨🇳`) is the regional
/// indicator path.  The third row mixes mono + emoji so chrome
/// switching between alpha + colour atlas mid-line is exercised.
#[test]
fn font_v5_emoji_color_snapshot() {
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
    // 4 rows × ~28pt — each pushes a different colour-emoji axis:
    //   L1  base emoji palette — single codepoint colour glyphs
    //   L2  ZWJ family clusters — multi-codepoint joined sequences
    //   L3  flag (regional indicator) pairs — 2 codepoint → 1 glyph
    //   L4  mixed alpha + colour line — atlas switching pressure
    let rows: &[(f64, &str)] = &[
        (28.0, "❤️  ⭐  🌈  🍕  🚀  🎉  🍎  ⚡  🦄  💎"),
        (28.0, "👨‍👩‍👧‍👦  👨‍👨‍👧  👩‍👩‍👦  🧑‍🚀  👨‍💻  👩‍🎨"),
        (28.0, "🇯🇵  🇰🇷  🇺🇸  🇨🇳  🇬🇧  🇩🇪  🇫🇷  🇮🇹  🇧🇷  🇮🇳"),
        (22.0, "Hello 👋 World 🌍, deploy 🚀 the 🏗️ build ✅"),
    ];
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    use crate::font_shape::ShapeOptions;
    let mut sheet = Sheet::new(2.0);
    let pad_x_pt = 20.0;
    // 28pt cap-height is ~22pt;  start the cursor far enough
    // down that the colour atlas slot doesn't run off the top.
    let mut y_pt = 36.0;
    let opts = ShapeOptions::full();
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    for (pt, line) in rows.iter() {
        let size_q = crate::glyph_atlas::GlyphKey::size_q_for(*pt);
        sheet
            .text(pad_x_pt, y_pt, line)
            .color(fg)
            .ui()
            .ui_size_q(size_q)
            .weight(400)
            .opts(opts)
            .draw();
        y_pt += pt * 1.55;
    }
    let bytes = renderer
        .render_sheet_to_bitmap(
            w_px,
            h_px,
            &sheet,
            chrome_cell_w,
            chrome_cell_h,
            chrome_ascent,
            true,
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
    let out_path = out_dir.join("font_v5_emoji_color.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 emoji color] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

/// Phase 9 (extended) — OpenType opts side-by-side fingerprint.
/// Phase 8 promises per-context OT feature toggle (`full()` /
/// `code()` / `all_off()`).  Showcase covers `full` vs `all_off`
/// on body text;  this snapshot adds `code()` as a third column
/// AND validates the diff is visible (so a regression that
/// silently collapses all 3 onto the same path surfaces as SSIM
/// hits across N rows simultaneously instead of going unnoticed).
///
/// Each row renders the same source string under all three
/// presets stacked vertically;  rows differ in which OT feature
/// they probe:
///   `Tax Wax Yes`  — kerning (off in `code` / `all_off`)
///   `fi fl ffi ffl`— `liga` (off only in `all_off`)
///   `==> != <==`   — `calt` contextual alts (Fira-style; mostly
///                    inert on SF Pro but the toggle stays)
///   `AVA WAV LTL`  — wide-kern letter triples
#[test]
fn font_v5_opentype_opts_snapshot() {
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
    let h_px: u32 = 900;
    let (chrome_cell_w, chrome_cell_h, chrome_ascent) = renderer.chrome_font_metrics();
    // 4 source rows × 3 OT presets = 12 rendered lines.
    let sources = [
        "Tax  Wax  Yes  AVA",
        "fi  fl  ffi  ffl",
        "==>  !=  <==  ===",
        "AVAVAV  WAWAWA  LTLTLT",
    ];
    let preset_pt: f64 = 22.0;
    let label_pt: f64 = 12.0;
    use crate::ui::core::Color;
    use super::sheet::Sheet;
    use crate::font_shape::ShapeOptions;
    let mut sheet = Sheet::new(2.0);
    let pad_x_pt = 20.0;
    let mut y_pt = 28.0;
    let row_gap_pt = preset_pt * 1.4;
    let group_gap_pt = 14.0;
    sheet
        .rect()
        .at(0.0, 0.0)
        .size(w_px as f64 / 2.0, h_px as f64 / 2.0)
        .fill(Color::rgba(15, 18, 23, 1.0))
        .draw();
    let fg = Color::rgba(220, 224, 235, 1.0);
    let fg_dim = Color::rgba(140, 150, 168, 1.0);
    let presets: &[(&str, ShapeOptions)] = &[
        ("full   ", ShapeOptions::full()),
        ("code   ", ShapeOptions::code()),
        ("all_off", ShapeOptions::all_off()),
    ];
    let label_size_q = crate::glyph_atlas::GlyphKey::size_q_for(label_pt);
    let preset_size_q = crate::glyph_atlas::GlyphKey::size_q_for(preset_pt);
    for source in sources.iter() {
        // Group separator label.
        sheet
            .text(pad_x_pt, y_pt, source)
            .color(fg_dim)
            .ui()
            .ui_size_q(label_size_q)
            .weight(400)
            .opts(ShapeOptions::full())
            .draw();
        y_pt += label_pt * 1.6;
        for (name, opts) in presets.iter() {
            let line = format!("{}  {}", name, source);
            sheet
                .text(pad_x_pt + 4.0, y_pt, &line)
                .color(fg)
                .ui()
                .ui_size_q(preset_size_q)
                .weight(400)
                .opts(*opts)
                .draw();
            y_pt += row_gap_pt;
        }
        y_pt += group_gap_pt;
    }
    let bytes = renderer
        .render_sheet_to_bitmap(
            w_px,
            h_px,
            &sheet,
            chrome_cell_w,
            chrome_cell_h,
            chrome_ascent,
            true,
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
    let out_path = out_dir.join("font_v5_opentype_opts.png");
    assert_snapshot_ssim(&rgba, &out_path, w_px, h_px, 0.98);
    write_snapshot_png(&out_path, &rgba, w_px, h_px);
    eprintln!(
        "[font v5 opentype opts] wrote {} ({} × {})",
        out_path.display(),
        w_px,
        h_px,
    );
}

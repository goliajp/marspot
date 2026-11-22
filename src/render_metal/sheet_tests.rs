//! Test sheets: alpha blending, submission order as z-order, and how
//! runs of one kind batch.

use super::*;

/// Semi-transparent fills must land at the alpha they were given.
///
/// The `ui_rect` fragment shader composites its layers into
/// **premultiplied** form (`rgb * a`), but the pipeline's source
/// blend factor was `SourceAlpha` — the *non*-premultiplied
/// equation — so the destination got alpha applied twice.  At
/// a = 1.0 the two agree, which is why every opaque panel looked
/// right and the bug stayed hidden; at a = 0.2 the fill rendered at
/// an effective 0.04 and simply vanished.  That is what made the cc
/// modal's dashed day grid and its status chips invisible, and it is
/// almost certainly what taught this codebase the folk rule that
/// overlay backgrounds "have to be" opaque.
///
/// White at alpha `a` over black must read `255 * a`, ± the SDF
/// edge AA. Sampled at the centre of a full-window rect, far from
/// any edge, so coverage is exactly 1.
#[test]
fn sheet_alpha_blends_at_the_requested_opacity() {
    use super::sheet::Sheet;
    use crate::ui::core::Color;
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => { eprintln!("SKIPPED: no Metal device: {e}"); return; }
    };
    let (w, h) = (16u32, 16u32);
    for (alpha, want) in [(1.0_f64, 255.0_f64), (0.5, 127.5), (0.25, 63.75)] {
        let mut sheet = Sheet::new(1.0);
        sheet.rect()
            .at(0.0, 0.0)
            .size(w as f64, h as f64)
            .fill(Color::rgba(255, 255, 255, alpha))
            .draw();
        let bytes = renderer
            .render_sheet_to_bitmap(w, h, &sheet, 48.0, 12.0, 9.0, false)
            .expect("render");
        let i = (w as usize / 2 + (h as usize / 2) * w as usize) * 4;
        let got = bytes[i + 2] as f64; // R channel of BGRA8
        assert!(
            (got - want).abs() <= 6.0,
            "alpha {alpha}: expected ~{want} over black, got {got} \
             (double-applied alpha would give {:.0})",
            want * alpha
        );
    }
}

/// Z-order invariant: three opaque rects at the SAME coord
/// in submission order red → green → blue must land blue on
/// the screen.  Same pipeline (all rects), so this verifies
/// the basic "last instance wins" within a single encoder.
#[test]
fn sheet_same_pipeline_submission_order_is_z_order() {
    use super::sheet::Sheet;
    use crate::ui::core::Color;
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => { eprintln!("SKIPPED: no Metal device: {e}"); return; }
    };
    let w = 8u32;
    let h = 8u32;
    let mut sheet = Sheet::new(1.0);
    // All three at (0, 0) sized full window.  Same pipeline
    // (Rect), so the encoder draws them in array order.
    sheet.rect()
        .at(0.0, 0.0)
        .size(w as f64, h as f64)
        .fill(Color::rgb(255, 0, 0))
        .draw();
    sheet.rect()
        .at(0.0, 0.0)
        .size(w as f64, h as f64)
        .fill(Color::rgb(0, 255, 0))
        .draw();
    sheet.rect()
        .at(0.0, 0.0)
        .size(w as f64, h as f64)
        .fill(Color::rgb(0, 0, 255))
        .draw();

    let bytes = renderer.render_sheet_to_bitmap(w, h, &sheet, 48.0, 12.0, 9.0, false)
        .expect("render");
    // Sample the centre pixel.  BGRA8: bytes are B, G, R, A.
    let px = w as usize / 2 + (h as usize / 2) * w as usize;
    let i = px * 4;
    let b = bytes[i];
    let g = bytes[i + 1];
    let r = bytes[i + 2];
    // Blue is the last-submitted: expect pure blue.
    assert!(b > 200, "expected blue dominant, got B={b} G={g} R={r}");
    assert!(g < 60,  "green should be near zero, got G={g}");
    assert!(r < 60,  "red should be near zero, got R={r}");
}

/// `sheet_runs` correctly merges consecutive same-kind
/// primitives into one run and splits at kind transitions.
#[test]
fn sheet_runs_batch_same_kind_split_on_transition() {
    use super::sheet::{RunKind, Sheet};
    use crate::ui::core::Color;
    let mut sheet = Sheet::new(1.0);
    sheet.rect().fill(Color::WHITE).draw();
    sheet.rect().fill(Color::BLACK).draw();
    sheet.text(0.0, 0.0, "x").color(Color::WHITE).draw();
    sheet.rect().fill(Color::WHITE).draw();
    // nothing is rendered: only the grouping into runs is read
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => { eprintln!("SKIPPED: no Metal device: {e}"); return; }
    };
    let drawn = renderer.sheet_runs(&sheet, 48.0, 12.0, 9.0, false);
    let (runs, gl) = (&drawn.runs, &drawn.glyphs);
    // Expected sequence: UiRect (2 rects) → Glyph (text) →
    // UiRect (final rect).  Text may produce 0 or 1+ glyphs
    // depending on whether 'x' resolves through the chrome
    // font — guard the assertion against the 0-glyph case
    // by collapsing the middle run.
    assert!(runs.len() >= 2, "expected at least 2 runs, got {runs:?}", runs = runs.len());
    assert_eq!(runs[0].0, RunKind::UiRect);
    assert_eq!(runs[0].1, 2);
    assert_eq!(runs.last().unwrap().0, RunKind::UiRect);
    // If the text produced glyphs, there's a Glyph run in the middle.
    if !gl.is_empty() {
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].0, RunKind::Glyph);
        assert_eq!(runs[1].1, gl.len());
    }
}

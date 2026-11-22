//! The offscreen passes: a headless renderer, the cell background and
//! glyph passes, and the rounded rect the shader reads.

use super::*;
use crate::frame_build::color::rgba8_of_f32;

/// Verify the basic Metal plumbing works on this machine — proves
/// the dep + bindings resolve and we can talk to the GPU.  CI on
/// non-Metal machines will skip this naturally because
/// `system_default_device` returns `Err`.
#[test]
fn headless_renderer_constructs() {
    match MetalRenderer::new_headless() {
        Ok(r) => {
            assert_eq!((r.width_px, r.height_px), (0.0, 0.0));
        }
        Err(e) => {
            eprintln!("skipping: no Metal device on this host ({e})");
        }
    }
}

/// End-to-end BG-pass test: render two solid-colour cells onto
/// a 4×4 texture and read back the pixels.  Verifies shader
/// compiles, pipeline state is built correctly, vertex/fragment
/// IO matches, and instanced draw + readback work.
#[test]
fn bg_pass_renders_cells() {
    let r = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(_) => {
            eprintln!("skipping: no Metal device");
            return;
        }
    };
    // 4×4 viewport, two cells:
    //   cell A at (0,0)-(2,4) → red    (left half)
    //   cell B at (2,0)-(2,4) → green  (right half)
    let cells = vec![
        CellInstance {
            origin: [0.0, 0.0],
            size: [2.0, 4.0],
            color: rgba8_of_f32([1.0, 0.0, 0.0, 1.0]),
        },
        CellInstance {
            origin: [2.0, 0.0],
            size: [2.0, 4.0],
            color: rgba8_of_f32([0.0, 1.0, 0.0, 1.0]),
        },
    ];
    let bytes = r
        .render_cells_bg_offscreen(4, 4, &cells)
        .expect("offscreen render");
    assert_eq!(bytes.len(), 4 * 4 * 4);

    // BGRA layout, top-left origin.  Sample one pixel from each
    // half — exact value depends on sRGB encoding so allow a wide
    // tolerance; we just need to see "left = red-ish, right = green-ish".
    let pixel_at = |x: usize, y: usize| {
        let off = (y * 4 + x) * 4;
        (bytes[off + 2], bytes[off + 1], bytes[off]) // R, G, B (skip A)
    };
    let (l_r, l_g, _l_b) = pixel_at(0, 1);
    let (r_r, r_g, _r_b) = pixel_at(3, 1);
    assert!(l_r > 200, "left half should be red, got R={l_r}");
    assert!(l_g < 50, "left half should be red, got G={l_g}");
    assert!(r_r < 50, "right half should be green, got R={r_r}");
    assert!(r_g > 200, "right half should be green, got G={r_g}");
}

/// End-to-end FG-pass test: rasterise glyph 'A' through the
/// real GlyphAtlas, then run a single FG draw of that glyph onto
/// a small texture and confirm the alpha-blended result is at
/// least somewhere in the destination.  Validates: shader compile,
/// FG pipeline state w/ blending, sampler, atlas-texture binding,
/// instanced draw + readback.
#[test]
fn fg_pass_renders_one_atlas_glyph() {
    use crate::frame_build::glyph_resolve::text_glyph_key;
    use core_text::font::new_from_name;

    let r = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(_) => {
            eprintln!("skipping: no Metal device");
            return;
        }
    };

    let font = new_from_name("Menlo", 13.0).expect("Menlo");
    let (mut atlas, atlas_texture) = new_atlas(
        &r.device,
        256,
        256,
        false,
        crate::font_cache::CoreTextFontTable::single(font.clone()),
    )
    .expect("atlas");

    let mut cg_glyph: core_graphics::font::CGGlyph = 0;
    let cu: u16 = b'A' as u16;
    unsafe {
        font.get_glyphs_for_characters(&cu, &mut cg_glyph, 1);
    }
    assert!(cg_glyph != 0);

    let entry = atlas
        .get_or_rasterize(
            text_glyph_key(0, cg_glyph, font.pt_size()),
            SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 },
            1,
        )
        .expect("rasterise A");
    let (atlas_w, atlas_h) = atlas.dims();

    // Two glyph instances, not one. A layout that strides by the
    // wrong number of bytes draws the first correctly and reads
    // the second from the wrong place -- the same trap that put a
    // green cell at G=0 when the flat rects moved, and one
    // instance cannot see it.
    //
    // Drawn at (8, 8) and (36, 8) with the atlas's pixel size and
    // a fully opaque white tint; the atlas's R8 alpha modulates
    // through to the readable colour.
    let one = GlyphInstance {
        origin: [8.0, 8.0],
        size: [entry.px_w as f32, entry.px_h as f32],
        uv0: [
            entry.u0 as f32 / atlas_w as f32,
            entry.v0 as f32 / atlas_h as f32,
        ],
        uv1: [
            entry.u1 as f32 / atlas_w as f32,
            entry.v1 as f32 / atlas_h as f32,
        ],
        color: rgba8_of_f32([1.0, 1.0, 1.0, 1.0]),
    };
    let glyphs = vec![one, GlyphInstance { origin: [36.0, 8.0], ..one }];

    let bytes = r
        .render_glyphs_fg_offscreen(64, 64, atlas_texture.texture(), &glyphs, (0.0, 0.0, 0.0, 1.0))
        .expect("fg offscreen");
    assert_eq!(bytes.len(), 64 * 64 * 4);
    // A glyph's soft edge over an opaque target leaves it opaque:
    // the window server composites the surface as it is.
    let soft = bytes.chunks_exact(4).filter(|p| p[0] > 8 && p[0] < 247).count();
    assert!(soft > 0, "no soft edge in the picture to judge by");
    assert!(bytes.chunks_exact(4).all(|p| p[3] == 255), "alpha under one where a glyph's edge was blended");

    // Find at least one pixel inside the glyph's destination
    // rect that's substantially brighter than the cleared
    // background (which was opaque black).
    let mut max_brightness = 0u8;
    for y in 8..(8 + entry.px_h as usize) {
        for x in 8..(8 + entry.px_w as usize) {
            let off = (y * 64 + x) * 4;
            let b = bytes[off];
            let g = bytes[off + 1];
            let r_byte = bytes[off + 2];
            let lum = ((b as u16 + g as u16 + r_byte as u16) / 3) as u8;
            if lum > max_brightness {
                max_brightness = lum;
            }
        }
    }
    assert!(
        max_brightness > 100,
        "expected at least one bright pixel inside glyph rect, got max={max_brightness}"
    );

    // And the second one, which is the instance that a wrong
    // stride loses.
    let mut second = 0u8;
    for y in 8..(8 + entry.px_h as usize) {
        for x in 36..(36 + entry.px_w as usize) {
            let off = (y * 64 + x) * 4;
            let lum = ((bytes[off] as u16
                + bytes[off + 1] as u16
                + bytes[off + 2] as u16)
                / 3) as u8;
            if lum > second {
                second = lum;
            }
        }
    }
    assert!(
        second > 100,
        "the second instance never drew: stride, got max={second}"
    );
}

/// The shader reads what the encoder writes.
///
/// There used to be two paths and a test that drew the same rects
/// through both: if an offset disagreed, the two bitmaps stopped
/// matching. There is one path now, so the comparison has nothing
/// to compare against and the question needs asking directly --
/// put a distinct value in every field and look at the pixels it
/// produces.
///
/// Each assertion names a field, so a layout that slips by four
/// bytes says which one moved rather than "the picture changed".
#[test]
fn the_shader_reads_what_the_encoder_writes() {
    let Ok(mut r) = MetalRenderer::new_headless() else {
        eprintln!("skipping: no Metal device on this host");
        return;
    };
    let (w, h) = (96u32, 96u32);
    let px = |buf: &[u8], x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        // BGRA, which is what the target format is.
        (buf[i + 2], buf[i + 1], buf[i], buf[i + 3])
    };
    let out = r
        .render_scene_rect(
            w,
            h,
            golia_ui_core::scene::UiRectInstance {
                origin: [24.0, 24.0],
                size: [48.0, 48.0],
                fill: golia_ui_core::Rgba8::rgba(200, 60, 40, 255),
                border: golia_ui_core::Rgba8::rgba(40, 200, 90, 255),
                radius: 0.0,
                border_width: 4.0,
                shadow_offset: [0.0, 0.0],
                shadow_color: golia_ui_core::Rgba8::TRANSPARENT,
                shadow_blur: 0.0,
            },
        )
        .expect("one rect");

    // `origin` and `size`: inside is painted, outside is not.
    assert_eq!(px(&out, 48, 48).0, 200, "fill red at the centre");
    // The pass clears to opaque black, which is also why a black
    // shadow on this target is invisible.
    assert_eq!(px(&out, 4, 4), (0, 0, 0, 255), "nothing outside the rect");
    // `fill`: the centre is the fill colour, not the border's.
    assert_eq!(px(&out, 48, 48), (200, 60, 40, 255), "fill colour");
    // `border` and `border_width`: the shader strokes inside, so
    // two pixels in from the edge is still border.
    assert_eq!(px(&out, 26, 48), (40, 200, 90, 255), "border colour");
    // ... and six in is past a four-pixel stroke.
    assert_eq!(px(&out, 30, 48), (200, 60, 40, 255), "border is 4px wide");
}

//! Rasterising a grapheme cluster, which is not a glyph.
//!
//! `rasterise_glyph` takes a glyph id.  A cluster may be a base plus a
//! mark the font anchors, a ZWJ sequence the font composed into one
//! glyph, or a regional-indicator pair CoreText fuses — so it is shaped
//! and drawn by CoreText rather than looked up.

use core_foundation::attributed_string::CFMutableAttributedString;
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::CFString;
use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGTextDrawingMode};
use core_text::font::CTFont;
use core_text::line::CTLine;
use core_text::string_attributes::kCTFontAttributeName;
use foreign_types::ForeignType;
use std::ffi::c_void;

use super::{CGBitmapContextCreate, CTLineDraw, SlotMetrics, KCGIMAGE_ALPHA_ONLY};

fn shaped(text: &str, font: &CTFont) -> CTLine {
    let cf_text = CFString::new(text);
    let mut attr = CFMutableAttributedString::new();
    attr.replace_str(&cf_text, CFRange::init(0, 0));
    let len = attr.char_len();
    unsafe {
        attr.set_attribute(CFRange::init(0, len), kCTFontAttributeName, &font.clone());
    }
    CTLine::new_with_attributed_string(attr.as_concrete_TypeRef())
}

/// Draw a whole grapheme cluster into an alpha bitmap.
///
/// The bitmap is cell-sized (`n_cells` wide) because that is the space
/// the cluster was measured to occupy; a cluster that overflows it is
/// clipped rather than scaled, which is what happens to a glyph whose
/// font disagrees with the width table and is the honest failure here
/// too.
pub fn rasterise_cluster(
    text: &str,
    font: &CTFont,
    metrics: SlotMetrics,
    n_cells: u16,
    buf: &mut [u8],
) -> bool {
    let px_w = metrics.cell_w * n_cells.max(1) as u32;
    let px_h = metrics.cell_h;
    if buf.len() < (px_w as usize) * (px_h as usize) {
        return false;
    }
    let line = shaped(text, font);

    let ctx = unsafe {
        let raw = CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut c_void,
            px_w as usize,
            px_h as usize,
            8,
            px_w as usize,
            std::ptr::null_mut(),
            KCGIMAGE_ALPHA_ONLY,
        );
        if raw.is_null() {
            return false;
        }
        CGContext::from_ptr(raw)
    };
    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_should_smooth_fonts(true);
    ctx.set_allows_font_smoothing(true);
    ctx.set_should_subpixel_position_fonts(true);
    ctx.set_allows_font_subpixel_positioning(true);
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
    ctx.set_gray_fill_color(1.0, 1.0);

    // The bitmap's origin is bottom-left and the baseline is measured
    // from the top, same as every other raster here.
    let baseline_from_bottom = px_h.saturating_sub(metrics.baseline_from_top) as f64;
    ctx.set_text_position(0.0, baseline_from_bottom);
    unsafe { CTLineDraw(line.as_concrete_TypeRef() as *const c_void, ctx.as_ptr()) };
    true
}

/// [`rasterise_cluster`] for a cluster in a colour font, into a
/// premultiplied BGRA bitmap for the colour atlas.
///
/// An alpha mask of a colour glyph is its silhouette: a ZWJ family
/// drawn that way is a filled square in the text colour, a flag a
/// white flag.  Placed the way `rasterise_glyph_color` places a single
/// emoji, so a family sits beside 🚀 at the same size: ink that
/// overflows the slot is scaled down to fit and centred.
pub fn rasterise_cluster_color(
    text: &str,
    font: &CTFont,
    metrics: SlotMetrics,
    n_cells: u16,
    buf: &mut [u8],
) -> bool {
    let px_w = metrics.cell_w * n_cells.max(1) as u32;
    let px_h = metrics.cell_h;
    let bytes_per_row = px_w as usize * 4;
    if buf.len() < bytes_per_row * px_h as usize {
        return false;
    }
    let line = shaped(text, font);

    let cs = CGColorSpace::create_device_rgb();
    let ctx = unsafe {
        let raw = CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut c_void,
            px_w as usize,
            px_h as usize,
            8,
            bytes_per_row,
            cs.as_ptr() as *mut c_void,
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little,
        );
        if raw.is_null() {
            return false;
        }
        CGContext::from_ptr(raw)
    };
    drop(cs);
    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);

    ctx.set_text_position(0.0, 0.0);
    let ink = line.get_image_bounds(&ctx);
    if ink.size.width <= 0.0 || ink.size.height <= 0.0 {
        return false;
    }
    let (slot_w, slot_h) = (px_w as f64, px_h as f64);
    if ink.size.width > slot_w || ink.size.height > slot_h {
        let scale = (slot_w / ink.size.width).min(slot_h / ink.size.height);
        ctx.scale(scale, scale);
        ctx.set_text_position(
            (slot_w / scale - ink.size.width) / 2.0 - ink.origin.x,
            (slot_h / scale - ink.size.height) / 2.0 - ink.origin.y,
        );
    } else {
        let baseline_from_bottom = px_h.saturating_sub(metrics.baseline_from_top) as f64;
        ctx.set_text_position(0.0, baseline_from_bottom);
    }
    unsafe { CTLineDraw(line.as_concrete_TypeRef() as *const c_void, ctx.as_ptr()) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An alpha mask cannot tell this test apart from a working path;
    /// colour can: the flag has a red disc and the family a blue tile,
    /// and a silhouette of either has no hue at all.
    #[test]
    fn a_colour_cluster_keeps_its_colours() {
        let Ok(mut font) = crate::font_cache::FontCache::build() else {
            eprintln!("skipping: no font stack");
            return;
        };
        let metrics = SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 };
        let drawn = |text: &str, font: &mut crate::font_cache::FontCache| {
            let base = marspot_term::grapheme::cluster_first_codepoint(text);
            let (idx, _) = font.resolve_char(base, false, false);
            assert!(font.is_color_font(idx), "{text:?} did not resolve to a colour font");
            let ct = font.font(idx).expect("the resolved font is in the table");
            let mut buf = vec![0u8; 32 * 32 * 4];
            assert!(rasterise_cluster_color(text, &ct, metrics, 2, &mut buf), "{text:?} drew nothing");
            buf
        };
        // BGRA, premultiplied
        let any = |buf: &[u8], f: &dyn Fn(i32, i32, i32) -> bool| {
            buf.chunks(4).any(|p| f(p[2] as i32, p[1] as i32, p[0] as i32))
        };
        let flag = drawn("\u{1f1ef}\u{1f1f5}", &mut font);
        assert!(any(&flag, &|r, g, b| r > 150 && g < 90 && b < 90), "the flag has no red");
        let family = drawn("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", &mut font);
        assert!(any(&family, &|r, _, b| b > r + 40), "the family has no blue");
    }
}

//! Pre-rasterised status banners.
//!
//! The shell can paint a short status string on top of whatever the
//! IOSurface is showing (or has last shown, frozen).  Used during
//! moments when the core is gone or about to come back — silent
//! upgrade in progress, a transient crash-restart, or the terminal
//! state where the crash budget tripped and we've stopped trying to
//! restart.
//!
//! This module rasterises the sentence (CoreText into a
//! CGBitmapContext) and hands back the image; the window shows it in
//! a layer of its own above the terminal's.
//!
//! A banner is rasterised when it goes up and again when the theme
//! changes under it — a few KB.

use core_foundation::attributed_string::CFMutableAttributedString;
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::CFString;
use core_graphics::base::kCGImageAlphaPremultipliedLast;
use core_graphics::color_space::{kCGColorSpaceSRGB, CGColorSpace};
use core_graphics::context::CGContext;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::image::CGImage;
use core_text::font;
use core_text::line::CTLine;
use core_text::string_attributes::kCTFontAttributeName;
use marspot::ui::strings::{t, Msg};

/// Which banner is currently active.  The shell pushes the active
/// kind into the presenter; `None` means "no banner, just composite
/// the IOSurface like usual."
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BannerKind {
    /// Crash budget has been blown, so restarts are paused for a
    /// cool-down.  Named for the state, not for a cause: it used to
    /// be `UpdateFailed`, and said "please restart the app", but the
    /// commonest way to reach it is a machine so loaded that four
    /// consecutive cores missed their handshake deadline — nothing
    /// failed to update, and quitting the app was never the fix.  The
    /// shell tries again on its own once the cool-down expires.
    RestartsPaused,
    /// Core died and we're spawning a fresh one.  Shown for the
    /// ~100 ms gap while the new process boots + attaches.
    ///
    /// Note: silent updates are *not* banner-worthy — the dual-core
    /// swap is invisible, so there's no "Updating…" state.
    Recovering,
    /// 2026-07-28 incident — this boot detected a crash-restart loop
    /// (≥5 launches in 5 min).  We came up in safe mode: sessions are
    /// reattached but nothing new is spawned and no extra windows are
    /// restored, so the loop cannot multiply processes.  Persistent
    /// for the whole boot; the user should look at the logs.
    CrashLoop,
}

impl BannerKind {
    /// The string we rasterise for this banner.
    pub fn text(self) -> &'static str {
        t(match self {
            BannerKind::RestartsPaused => Msg::BannerRestartsPaused,
            BannerKind::Recovering => Msg::BannerRecovering,
            BannerKind::CrashLoop => Msg::BannerCrashLoop,
        })
    }
}

/// A banner on screen: which, and the display scale its raster was
/// made for.  A window that moves to a display of another scale shows
/// another raster of the same banner, so either differing means
/// drawing it again.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Shown {
    pub kind: BannerKind,
    pub scale: f64,
}

/// A rasterised banner and its size, so the layer that shows it can be
/// sized without measuring again.
pub struct BannerImage {
    pub image: CGImage,
    /// Physical pixels.
    pub width_px: u32,
    pub height_px: u32,
}

/// Rasterise `text` with CoreText: premultiplied-alpha RGBA8 in sRGB,
/// the colour space the terminal's own frames are shown in.
pub fn rasterise(text: &str, point_size: f64, scale: f64) -> Result<BannerImage, String> {
    // Build a CTFont — system default sans, regular weight.
    let ct_font = font::new_from_name("HelveticaNeue", point_size * scale).map_err(|()| {
        format!("CTFont new_from_name HelveticaNeue {} pt failed", point_size)
    })?;

    // Wrap the input in a mutable CFAttributedString and tag every
    // character with the font.  CFAttributedString (immutable) only
    // takes a string and doesn't expose attribute setters in this
    // crate.
    let cf_string = CFString::new(text);
    let mut attr = CFMutableAttributedString::new();
    attr.replace_str(&cf_string, CFRange { location: 0, length: 0 });
    let char_len = attr.char_len();
    unsafe {
        attr.set_attribute(
            CFRange {
                location: 0,
                length: char_len,
            },
            kCTFontAttributeName,
            &ct_font,
        );
    }
    marspot::font_shape::tag_language(&mut attr, char_len);
    let line = CTLine::new_with_attributed_string(attr.as_concrete_TypeRef());

    let bounds = line.get_typographic_bounds();
    let width_pt: f64 = bounds.width;
    let ascent: f64 = bounds.ascent;
    let descent: f64 = bounds.descent;

    // Add some breathing-room padding so the corners of the box show
    // a small inset; CoreText measurements only cover ink, not the
    // visual cushion users expect.
    let pad_x: f64 = 16.0 * scale;
    let pad_y: f64 = 8.0 * scale;
    let img_w = (width_pt.ceil() + 2.0 * pad_x) as u32;
    let img_h = ((ascent + descent).ceil() + 2.0 * pad_y) as u32;
    if img_w == 0 || img_h == 0 {
        return Err("banner rasterise: zero-size measurement".to_string());
    }

    // CoreGraphics bitmap context to draw into.  RGBA premultiplied.
    // SAFETY: a constant name CoreGraphics exports for the life of the process.
    let color_space = CGColorSpace::create_with_name(unsafe { kCGColorSpaceSRGB })
        .ok_or_else(|| "banner rasterise: no sRGB colour space".to_string())?;
    let ctx = CGContext::create_bitmap_context(
        None,
        img_w as usize,
        img_h as usize,
        8,
        (img_w as usize) * 4,
        &color_space,
        kCGImageAlphaPremultipliedLast,
    );
    // Background: the theme's notice plate, translucent where the
    // theme lets terminal content show through, and never the cell BG
    // so it reads as "a notice" rather than "the terminal".
    let [r, g, b, a] = marspot::ui::components::panel_palette::notice_bg();
    ctx.set_rgb_fill_color(r as f64, g as f64, b as f64, a as f64);
    ctx.fill_rect(CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(img_w as f64, img_h as f64),
    ));

    // Foreground: the theme's notice text.
    let [r, g, b, a] = marspot::ui::components::panel_palette::notice_fg();
    ctx.set_rgb_fill_color(r as f64, g as f64, b as f64, a as f64);
    // CoreText's baseline is from the bottom of the bitmap; this
    // origin puts ink ~descent + pad above bottom.
    ctx.set_text_position(pad_x, pad_y + descent);
    line.draw(&ctx);

    let image = ctx.create_image().ok_or_else(|| "banner rasterise: no image from the bitmap".to_string())?;
    Ok(BannerImage {
        image,
        width_px: img_w,
        height_px: img_h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_banner_drawn_for_another_display_scale_is_not_the_one_shown() {
        let shown = |kind, scale| Some(Shown { kind, scale });
        let up = shown(BannerKind::Recovering, 1.0);
        assert_eq!(up, shown(BannerKind::Recovering, 1.0));
        assert_ne!(up, shown(BannerKind::Recovering, 2.0), "the window moved to a denser display");
        assert_ne!(up, shown(BannerKind::CrashLoop, 1.0));
        assert_ne!(up, None);
    }

    /// The raster is the size the scale asks for: the same sentence at
    /// scale 2 is twice as wide and as tall, to the pixel the two
    /// roundings can differ by.
    #[test]
    fn the_raster_is_twice_the_size_at_twice_the_scale() {
        let one = rasterise(BannerKind::Recovering.text(), 14.0, 1.0).expect("a banner at scale 1");
        let two = rasterise(BannerKind::Recovering.text(), 14.0, 2.0).expect("a banner at scale 2");
        assert_eq!((one.image.width(), one.image.height()), (one.width_px as usize, one.height_px as usize));
        assert!(one.width_px > 40 && one.height_px > 16, "{} x {}", one.width_px, one.height_px);
        assert!(two.width_px.abs_diff(one.width_px * 2) <= 2, "{} against {}", two.width_px, one.width_px);
        assert!(two.height_px.abs_diff(one.height_px * 2) <= 2, "{} against {}", two.height_px, one.height_px);
    }
}

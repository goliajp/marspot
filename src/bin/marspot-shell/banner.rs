//! Pre-rasterised status banners.
//!
//! The shell can paint a short status string on top of whatever the
//! IOSurface is showing (or has last shown, frozen).  Used during
//! moments when the core is gone or about to come back — silent
//! upgrade in progress, a transient crash-restart, or the terminal
//! state where the crash budget tripped and we've stopped trying to
//! restart.
//!
//! Architecturally this module owns the **CPU-side** rasterisation
//! (CoreText into a CGBitmapContext) plus the upload to an
//! `MTLTexture`.  The `ShellPresenter` owns the **GPU-side** blend
//! pipeline that draws the resulting texture as an overlay.
//!
//! Each `BannerKind` rasterises once at first use and the texture is
//! cached for the life of the shell — they're a few KB each and the
//! upload is the only nontrivial cost.

use core_foundation::attributed_string::CFMutableAttributedString;
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::CFString;
use core_graphics::base::kCGImageAlphaPremultipliedLast;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_text::font;
use core_text::line::CTLine;
use core_text::string_attributes::kCTFontAttributeName;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLRegion, MTLStorageMode, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};

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
    /// The string we rasterise for this banner.  Hard-coded English
    /// for now; matches marspot's existing convention of being a
    /// developer tool first, polished UI later.
    pub fn text(self) -> &'static str {
        match self {
            BannerKind::RestartsPaused => "Marspot's core keeps stopping — it will retry on its own",
            BannerKind::Recovering => "Marspot is recovering…",
            BannerKind::CrashLoop => {
                "Marspot crashed repeatedly — safe mode (sessions kept, nothing new spawned)"
            }
        }
    }
}

/// CPU-rendered banner with a side car of dimensions, so the GPU
/// side can position the quad without re-measuring.
pub struct BannerTexture {
    pub texture: Retained<ProtocolObject<dyn MTLTexture>>,
    /// Physical pixel size — used to convert to NDC at present time.
    pub width_px: u32,
    pub height_px: u32,
}

/// Rasterise `text` to an MTLTexture using CoreText.  The texture is
/// premultiplied-alpha RGBA8.  Returns the texture + its pixel dims.
pub fn rasterise(
    device: &ProtocolObject<dyn MTLDevice>,
    text: &str,
    point_size: f64,
    scale: f64,
) -> Result<BannerTexture, String> {
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
    let color_space = CGColorSpace::create_device_rgb();
    let mut ctx = CGContext::create_bitmap_context(
        None,
        img_w as usize,
        img_h as usize,
        8,
        (img_w as usize) * 4,
        &color_space,
        kCGImageAlphaPremultipliedLast,
    );
    // Background: dark grey with ~85 % alpha so terminal content
    // shows through.  Chosen warmer than the cell BG so it reads
    // as "a notice" rather than "the terminal".
    ctx.set_rgb_fill_color(0.18, 0.20, 0.24, 0.85);
    ctx.fill_rect(CGRect::new(
        &CGPoint::new(0.0, 0.0),
        &CGSize::new(img_w as f64, img_h as f64),
    ));

    // Foreground: near-white with a slight warmth so it reads as
    // "marspot accent" rather than "system error".
    ctx.set_rgb_fill_color(0.97, 0.95, 0.91, 1.0);
    // CoreText's baseline is from the bottom of the bitmap; this
    // origin puts ink ~descent + pad above bottom.
    ctx.set_text_position(pad_x, pad_y + descent);
    line.draw(&ctx);

    // Hand off to Metal.
    let texture = upload_bitmap(device, &mut ctx, img_w, img_h)?;
    Ok(BannerTexture {
        texture,
        width_px: img_w,
        height_px: img_h,
    })
}

fn upload_bitmap(
    device: &ProtocolObject<dyn MTLDevice>,
    ctx: &mut CGContext,
    width: u32,
    height: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::RGBA8Unorm,
            width as usize,
            height as usize,
            false,
        )
    };
    descriptor.setUsage(MTLTextureUsage::ShaderRead);
    descriptor.setStorageMode(MTLStorageMode::Shared);
    let tex = device
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| "newTextureWithDescriptor for banner returned nil".to_string())?;
    let bytes = ctx.data();
    let region = MTLRegion {
        origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: objc2_metal::MTLSize {
            width: width as usize,
            height: height as usize,
            depth: 1,
        },
    };
    let bytes_per_row = (width * 4) as usize;
    unsafe {
        let nn = std::ptr::NonNull::new(bytes.as_ptr() as *mut std::ffi::c_void)
            .ok_or_else(|| "banner bitmap data ptr was null".to_string())?;
        tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, nn, bytes_per_row);
    }
    Ok(tex)
}

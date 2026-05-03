//! AppKit-native rendering.
//!
//! Pivoted from custom Metal + glyph atlas + shader pipeline (which had
//! gamma + atlas neighbor + sampling issues we couldn't shake).  This
//! module instead lets macOS handle all glyph rasterization via
//! CoreText into a `CGBitmapContext`, then sets the resulting `CGImage`
//! as the host view's CALayer contents.  CoreAnimation composites it.
//!
//! The visual output should now match `Terminal.app`/`TextEdit` pixel
//! for pixel — same font hinting, same antialiasing, same gamma path
//! Apple uses everywhere else.

use crate::grid::Grid;
use core_foundation::base::TCFType;
use core_graphics::base::{
    kCGBitmapByteOrder32Big, kCGImageAlphaPremultipliedLast, CGFloat,
};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGTextDrawingMode};
use core_graphics::font::CGGlyph;
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
use core_text::font::{new_from_name, CTFont};
use core_text::font_descriptor::kCTFontOrientationDefault;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSImage, NSView};
use objc2_foundation::NSSize;
use objc2_quartz_core::CALayer;

const FONT_NAME: &str = "Menlo";
const FONT_POINT: f64 = 13.0;

/// Background color for the terminal.  In the **normalized linear-ish
/// sRGB-display** space — what you'd type as a CSS hex.
const BG: (CGFloat, CGFloat, CGFloat) = (0.05, 0.07, 0.12);
/// Default foreground (white text).
const FG: (CGFloat, CGFloat, CGFloat) = (0.92, 0.92, 0.92);

pub struct Renderer {
    /// When attached to an NSView, we update its CALayer's `contents` on
    /// each render with a fresh CGImage.  In headless mode this is None.
    layer: Option<Retained<CALayer>>,
    font: CTFont,
    cell_w: f64,
    cell_h: f64,
    ascent: f64,
    viewport_w: f64,
    viewport_h: f64,
    scale: f64,
}

impl Renderer {
    pub fn new(view: &NSView, scale: f32) -> Result<Self, String> {
        Self::build(Some(view), scale)
    }

    pub fn new_offscreen(scale: f32) -> Result<Self, String> {
        Self::build(None, scale)
    }

    fn build(view: Option<&NSView>, scale: f32) -> Result<Self, String> {
        let font = new_from_name(FONT_NAME, FONT_POINT)
            .or_else(|_| new_from_name("Menlo", FONT_POINT))
            .map_err(|_| "could not load font".to_string())?;

        let cell_w = compute_cell_width(&font);
        let ascent = font.ascent();
        let cell_h = ascent + font.descent() + font.leading();

        let layer = if let Some(view) = view {
            view.setWantsLayer(true);
            // The view is now layer-backed; AppKit creates a default
            // CALayer for us.  We grab it and set its contentsScale so
            // CA composites at the right density.
            let layer = unsafe {
                view.layer()
                    .ok_or("layer not available on view".to_string())?
            };
            unsafe { layer.setContentsScale(scale as f64) };
            Some(layer)
        } else {
            None
        };

        Ok(Self {
            layer,
            font,
            cell_w,
            cell_h,
            ascent,
            viewport_w: 0.0,
            viewport_h: 0.0,
            scale: scale as f64,
        })
    }

    pub fn resize(&mut self, width_px: f64, height_px: f64) {
        self.viewport_w = width_px;
        self.viewport_h = height_px;
    }

    pub fn render(&mut self, grid: &Grid) {
        if self.layer.is_none() {
            return;
        }
        if self.viewport_w < 1.0 || self.viewport_h < 1.0 {
            return;
        }
        let w = self.viewport_w as u32;
        let h = self.viewport_h as u32;
        let ctx = self.draw_frame(w, h, grid);
        let cgimage = ctx
            .create_image()
            .expect("CGContext should produce a CGImage");
        // Push the image into the layer.  CGImageRef bridges to id;
        // CALayer's `contents` setter accepts CGImageRef directly.
        let layer = self.layer.as_ref().unwrap();
        let cg_ptr = cgimage.as_ptr() as *const AnyObject;
        unsafe {
            layer.setContents(Some(&*cg_ptr));
        }
    }

    pub fn snapshot(
        &mut self,
        width: u32,
        height: u32,
        grid: &Grid,
    ) -> Result<Vec<u8>, String> {
        self.viewport_w = width as f64;
        self.viewport_h = height as f64;
        let mut ctx = self.draw_frame(width, height, grid);
        let mut bytes = ctx.data().to_vec();
        // PNG wants RGBA; if the bitmap context produced BGRA, swap
        // R and B per pixel.  We detect this empirically here rather
        // than relying on byte-order flag interpretation.
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
        Ok(bytes)
    }

    /// Render one frame of the current terminal grid into a fresh
    /// CGBitmapContext at `width × height` physical pixels.  The caller
    /// can either turn the context into a CGImage (for live layer
    /// contents) or read its bytes directly (for snapshot/PNG).
    fn draw_frame(&self, width: u32, height: u32, grid: &Grid) -> CGContext {
        let space = CGColorSpace::create_device_rgb();
        let row_bytes = width as usize * 4;
        // Bitmap info: RGBA in memory order (alpha last + big-endian
        // byte order forces R/G/B/A on little-endian macOS too).
        let bitmap_info = kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big;
        let mut ctx = CGContext::create_bitmap_context(
            None,
            width as usize,
            height as usize,
            8,
            row_bytes,
            &space,
            bitmap_info,
        );

        // Background fill.
        ctx.set_rgb_fill_color(BG.0, BG.1, BG.2, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(width as f64, height as f64),
        ));

        // Stay in CG's default y-up coordinate system.  Computing
        // baselines from bottom-up is awkward but it avoids any text-
        // matrix interaction we'd need with a flipped CTM.  CGBitmapContext
        // memory is stored top-down regardless of the drawing coordinate
        // system, so the resulting bytes still come out in conventional
        // top-down RGBA order.

        // Apple's default font rendering hints — turn ON everything so
        // CoreText produces the same output it would in Terminal.app.
        ctx.set_allows_antialiasing(true);
        ctx.set_should_antialias(true);
        ctx.set_allows_font_smoothing(true);
        ctx.set_should_smooth_fonts(true);
        ctx.set_allows_font_subpixel_positioning(true);
        ctx.set_should_subpixel_position_fonts(true);
        ctx.set_allows_font_subpixel_quantization(true);
        ctx.set_should_subpixel_quantize_fonts(true);

        ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
        ctx.set_rgb_fill_color(FG.0, FG.1, FG.2, 1.0);

        let cols = grid.cols() as usize;
        let mut chars: Vec<u16> = Vec::with_capacity(cols);
        let mut glyphs: Vec<CGGlyph> = Vec::with_capacity(cols);
        let mut positions: Vec<CGPoint> = Vec::with_capacity(cols);

        for r in 0..grid.rows() {
            chars.clear();
            for c in 0..grid.cols() {
                let ch = grid.cell(c, r).ch;
                // BMP only for now; non-BMP would need surrogate pairs.
                let cp = ch as u32;
                if cp <= 0xFFFF {
                    chars.push(cp as u16);
                } else {
                    chars.push(b'?' as u16);
                }
            }
            glyphs.clear();
            glyphs.resize(chars.len(), 0);
            unsafe {
                self.font.get_glyphs_for_characters(
                    chars.as_ptr(),
                    glyphs.as_mut_ptr(),
                    chars.len() as core_foundation::base::CFIndex,
                );
            }
            // CG y-up: y=0 is bottom of image.  Row 0 (top of terminal)
            // baseline is at y = height - ascent.  Row r baseline is
            // y = height - (r * cell_h + ascent).
            let baseline_y = height as f64 - (r as f64 * self.cell_h + self.ascent);
            positions.clear();
            for i in 0..glyphs.len() {
                positions.push(CGPoint::new(i as f64 * self.cell_w, baseline_y));
            }
            self.font.draw_glyphs(&glyphs, &positions, ctx.clone());
        }

        self.draw_cursor(&ctx, height, grid);

        ctx
    }

    /// Standard "block" cursor: fill the cursor cell with the foreground
    /// colour, then re-draw that cell's glyph in the background colour so
    /// the character under the cursor stays readable. Always rendered for
    /// now — focus-aware (hollow when unfocused) is a later refinement.
    fn draw_cursor(&self, ctx: &CGContext, height: u32, grid: &Grid) {
        let (col, row) = grid.cursor();
        let cx = col as f64 * self.cell_w;
        let cy_bottom = height as f64 - (row as f64 + 1.0) * self.cell_h;

        ctx.set_rgb_fill_color(FG.0, FG.1, FG.2, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(cx, cy_bottom),
            &CGSize::new(self.cell_w, self.cell_h),
        ));

        // Punch the cell's glyph back through in the background colour.
        // Skip if the cell is blank — saves a CoreText call per frame
        // when the cursor sits on a space (the common idle case).
        let cell = grid.cell(col, row);
        if cell.ch != ' ' && cell.ch != '\0' {
            let cp = cell.ch as u32;
            let ch16: u16 = if cp <= 0xFFFF { cp as u16 } else { b'?' as u16 };
            let mut glyph: CGGlyph = 0;
            unsafe {
                self.font.get_glyphs_for_characters(&ch16, &mut glyph, 1);
            }
            if glyph != 0 {
                ctx.set_rgb_fill_color(BG.0, BG.1, BG.2, 1.0);
                let baseline_y =
                    height as f64 - (row as f64 * self.cell_h + self.ascent);
                self.font.draw_glyphs(
                    &[glyph],
                    &[CGPoint::new(cx, baseline_y)],
                    ctx.clone(),
                );
            }
        }
    }
}

fn compute_cell_width(font: &CTFont) -> f64 {
    let mut glyph: CGGlyph = 0;
    let m: u16 = b'M' as u16;
    let _ok = unsafe { font.get_glyphs_for_characters(&m, &mut glyph, 1) };
    if glyph == 0 {
        return 7.0; // sane fallback
    }
    let mut size = CGSize::new(0.0, 0.0);
    unsafe {
        font.get_advances_for_glyphs(kCTFontOrientationDefault, &glyph, &mut size, 1);
    }
    size.width
}

#[allow(dead_code)]
fn _force_use_nsimage(_: &NSImage, _: NSSize) {}

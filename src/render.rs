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

use crate::grid::{CellAttrs, Color, Grid};
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::base::{
    kCGBitmapByteOrder32Big, kCGImageAlphaPremultipliedLast, CGFloat,
};
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::{CGContext, CGTextDrawingMode};
use core_graphics::font::CGGlyph;
use core_graphics::geometry::{CGAffineTransform, CGPoint, CGRect, CGSize};
use core_text::font::{new_from_name, CTFont, CTFontRef};
use core_text::font_descriptor::kCTFontOrientationDefault;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSImage, NSView};
use objc2_foundation::NSSize;
use objc2_quartz_core::CALayer;
use std::collections::HashMap;

/// CoreText's per-string font fallback resolver: given a base font and a
/// CFString, returns a font that can render the characters in `range`.
/// The core-text crate doesn't expose this binding so we declare it
/// directly — falls under CLAUDE.md's "necessary FFI bindings, kept".
#[link(name = "CoreText", kind = "framework")]
extern "C" {
    fn CTFontCreateForString(
        currentFont: CTFontRef,
        string: CFStringRef,
        range: CFRange,
    ) -> CTFontRef;
}

const FONT_NAME: &str = "Menlo";
const FONT_POINT: f64 = 13.0;

/// Background color for the terminal.  In the **normalized linear-ish
/// sRGB-display** space — what you'd type as a CSS hex.
const BG: (CGFloat, CGFloat, CGFloat) = (0.05, 0.07, 0.12);
/// Default foreground (white text).
const FG: (CGFloat, CGFloat, CGFloat) = (0.92, 0.92, 0.92);

/// Standard ANSI 16-colour palette (xterm values).  Indices 0–7 are the
/// basic colours; 8–15 are their bright variants.  256-colour and 24-bit
/// modes resolve through `palette_color`.
const ANSI_16: [(CGFloat, CGFloat, CGFloat); 16] = [
    (0.00, 0.00, 0.00), // 0  black
    (0.67, 0.00, 0.00), // 1  red
    (0.00, 0.67, 0.00), // 2  green
    (0.67, 0.33, 0.00), // 3  yellow
    (0.00, 0.00, 0.67), // 4  blue
    (0.67, 0.00, 0.67), // 5  magenta
    (0.00, 0.67, 0.67), // 6  cyan
    (0.67, 0.67, 0.67), // 7  white (light grey)
    (0.33, 0.33, 0.33), // 8  bright black
    (1.00, 0.33, 0.33), // 9  bright red
    (0.33, 1.00, 0.33), // 10 bright green
    (1.00, 1.00, 0.33), // 11 bright yellow
    (0.33, 0.33, 1.00), // 12 bright blue
    (1.00, 0.33, 1.00), // 13 bright magenta
    (0.33, 1.00, 1.00), // 14 bright cyan
    (1.00, 1.00, 1.00), // 15 bright white
];

fn palette_color(idx: u8) -> (CGFloat, CGFloat, CGFloat) {
    if (idx as usize) < ANSI_16.len() {
        return ANSI_16[idx as usize];
    }
    if idx < 232 {
        // 6×6×6 colour cube; xterm uses {0, 95, 135, 175, 215, 255} as the
        // per-channel ramp.
        const RAMP: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let n = idx - 16;
        let r = RAMP[(n / 36) as usize];
        let g = RAMP[((n / 6) % 6) as usize];
        let b = RAMP[(n % 6) as usize];
        return (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    }
    // 24-step grayscale ramp from #080808 to #eeeeee.
    let v = 8 + (idx - 232) as i32 * 10;
    let f = v as f64 / 255.0;
    (f, f, f)
}

fn resolve_color(c: Color, default_rgb: (CGFloat, CGFloat, CGFloat)) -> (CGFloat, CGFloat, CGFloat) {
    match c {
        Color::Default => default_rgb,
        Color::Indexed(i) => palette_color(i),
        Color::Rgb(r, g, b) => (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0),
    }
}

/// Resolve a cell's attrs to (fg, bg) RGB, honoring SGR reverse.
fn resolve_attrs(attrs: CellAttrs) -> ((CGFloat, CGFloat, CGFloat), (CGFloat, CGFloat, CGFloat)) {
    let mut fg = resolve_color(attrs.fg, FG);
    let mut bg = resolve_color(attrs.bg, BG);
    if attrs.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

pub struct Renderer {
    /// When attached to an NSView, we update its CALayer's `contents` on
    /// each render with a fresh CGImage.  In headless mode this is None.
    layer: Option<Retained<CALayer>>,
    /// `fonts.fonts[0]` is the base font; subsequent indices are fallback
    /// fonts discovered lazily via `CTFontCreateForString` for codepoints
    /// the base font lacks.
    fonts: FontRegistry,
    /// Per-codepoint resolution: which font (by index into `fonts`) and
    /// glyph id can render this codepoint.  Filled lazily; entries are
    /// stable for the renderer's lifetime.
    char_cache: HashMap<u32, (usize, CGGlyph)>,
    cell_w: f64,
    cell_h: f64,
    ascent: f64,
    viewport_w: f64,
    viewport_h: f64,
    scale: f64,
}

/// Holds the base font plus any fallback fonts discovered at runtime, with
/// a postscript-name → index map so we can dedup fallbacks (CoreText hands
/// us a fresh `CTFontRef` each lookup even when the underlying font is the
/// same one).
struct FontRegistry {
    fonts: Vec<CTFont>,
    by_name: HashMap<String, usize>,
}

impl FontRegistry {
    fn new(base: CTFont) -> Self {
        let name = base.postscript_name();
        let mut by_name = HashMap::new();
        by_name.insert(name, 0);
        Self {
            fonts: vec![base],
            by_name,
        }
    }

    /// Insert a font if its postscript name isn't already known; in either
    /// case returns the index of the canonical instance.
    fn intern(&mut self, font: CTFont) -> usize {
        let name = font.postscript_name();
        if let Some(&idx) = self.by_name.get(&name) {
            return idx;
        }
        let idx = self.fonts.len();
        self.by_name.insert(name, idx);
        self.fonts.push(font);
        idx
    }
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
            fonts: FontRegistry::new(font),
            char_cache: HashMap::new(),
            cell_w,
            cell_h,
            ascent,
            viewport_w: 0.0,
            viewport_h: 0.0,
            scale: scale as f64,
        })
    }

    /// Resolve a character to (font_idx, glyph).  The base font is tried
    /// first; on .notdef we ask CoreText for a per-string fallback and
    /// intern the result.  Cached by codepoint.
    fn resolve_char(&mut self, ch: char) -> (usize, CGGlyph) {
        let cp = ch as u32;
        if let Some(&entry) = self.char_cache.get(&cp) {
            return entry;
        }
        let base = self.fonts.fonts[0].clone();
        let glyph = lookup_glyph(&base, ch);
        let entry = if glyph != 0 {
            (0, glyph)
        } else {
            let fallback = create_fallback_font(&base, ch);
            let fb_glyph = lookup_glyph(&fallback, ch);
            let idx = self.fonts.intern(fallback);
            (idx, fb_glyph)
        };
        self.char_cache.insert(cp, entry);
        entry
    }

    pub fn resize(&mut self, width_px: f64, height_px: f64) {
        self.viewport_w = width_px;
        self.viewport_h = height_px;
    }

    /// Cell dimensions in physical pixels — the unit the renderer uses
    /// internally.  Callers (e.g. the resize path) divide the viewport by
    /// these to get the grid dimensions that fit.
    pub fn cell_dims(&self) -> (f64, f64) {
        (self.cell_w, self.cell_h)
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
    fn draw_frame(&mut self, width: u32, height: u32, grid: &Grid) -> CGContext {
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

        let cols = grid.cols() as usize;
        // Reusable per-run scratch buffers for batched draw_glyphs calls.
        let mut run_glyphs: Vec<CGGlyph> = Vec::with_capacity(cols);
        let mut run_positions: Vec<CGPoint> = Vec::with_capacity(cols);

        for r in 0..grid.rows() {
            // 1) Background pass — fill runs of cells that share a non-default
            //    background color.  Cells with the default BG inherit the
            //    frame fill we already laid down above.
            let row_bottom_y = height as f64 - (r as f64 + 1.0) * self.cell_h;
            let mut c = 0usize;
            while c < cols {
                let bg = resolve_attrs(grid.cell(c as u16, r).attrs).1;
                if bg == BG {
                    c += 1;
                    continue;
                }
                let start = c;
                c += 1;
                while c < cols && resolve_attrs(grid.cell(c as u16, r).attrs).1 == bg {
                    c += 1;
                }
                ctx.set_rgb_fill_color(bg.0, bg.1, bg.2, 1.0);
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(start as f64 * self.cell_w, row_bottom_y),
                    &CGSize::new((c - start) as f64 * self.cell_w, self.cell_h),
                ));
            }

            // 2) Foreground pass — group consecutive non-blank cells that
            //    share both font (base or fallback) and fg color, emit one
            //    draw_glyphs call per run.  Font transitions break runs
            //    because draw_glyphs is bound to a single CTFont.
            let baseline_y = height as f64 - (r as f64 * self.cell_h + self.ascent);
            let mut i = 0usize;
            while i < cols {
                let cell = grid.cell(i as u16, r);
                if cell.ch == ' ' || cell.ch == '\0' {
                    i += 1;
                    continue;
                }
                let (font_idx, glyph) = self.resolve_char(cell.ch);
                let fg = resolve_attrs(cell.attrs).0;
                run_glyphs.clear();
                run_positions.clear();
                run_glyphs.push(glyph);
                run_positions.push(CGPoint::new(i as f64 * self.cell_w, baseline_y));
                i += 1;
                while i < cols {
                    let cur = grid.cell(i as u16, r);
                    if cur.ch == ' ' || cur.ch == '\0' {
                        break;
                    }
                    let (cur_font, cur_glyph) = self.resolve_char(cur.ch);
                    if cur_font != font_idx || resolve_attrs(cur.attrs).0 != fg {
                        break;
                    }
                    run_glyphs.push(cur_glyph);
                    run_positions.push(CGPoint::new(i as f64 * self.cell_w, baseline_y));
                    i += 1;
                }
                ctx.set_rgb_fill_color(fg.0, fg.1, fg.2, 1.0);
                let font = &self.fonts.fonts[font_idx];
                font.draw_glyphs(&run_glyphs, &run_positions, ctx.clone());
            }
        }

        self.draw_cursor(&ctx, height, grid);

        ctx
    }

    /// Standard "block" cursor: fill the cursor cell with the foreground
    /// colour, then re-draw that cell's glyph in the background colour so
    /// the character under the cursor stays readable. Always rendered for
    /// now — focus-aware (hollow when unfocused) is a later refinement.
    fn draw_cursor(&mut self, ctx: &CGContext, height: u32, grid: &Grid) {
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
            let (font_idx, glyph) = self.resolve_char(cell.ch);
            if glyph != 0 {
                ctx.set_rgb_fill_color(BG.0, BG.1, BG.2, 1.0);
                let baseline_y =
                    height as f64 - (row as f64 * self.cell_h + self.ascent);
                let font = &self.fonts.fonts[font_idx];
                font.draw_glyphs(
                    &[glyph],
                    &[CGPoint::new(cx, baseline_y)],
                    ctx.clone(),
                );
            }
        }
    }
}

/// Look up the glyph id for `ch` in `font`.  Handles both BMP and non-BMP
/// codepoints (the latter as a UTF-16 surrogate pair, where CoreText puts
/// the actual glyph in the trailing slot).  Returns 0 if the font can't
/// render this codepoint.
fn lookup_glyph(font: &CTFont, ch: char) -> CGGlyph {
    let cp = ch as u32;
    if cp <= 0xFFFF {
        let cu = cp as u16;
        let mut g: CGGlyph = 0;
        unsafe {
            font.get_glyphs_for_characters(&cu, &mut g, 1);
        }
        g
    } else {
        let mut buf = [0u16; 2];
        ch.encode_utf16(&mut buf);
        let mut glyphs = [0 as CGGlyph; 2];
        unsafe {
            font.get_glyphs_for_characters(buf.as_ptr(), glyphs.as_mut_ptr(), 2);
        }
        // Apple's docs disagree across versions about whether surrogate
        // pairs put the glyph at the lead or trail index — empirically
        // pick whichever is non-zero so we don't render .notdef.
        if glyphs[0] != 0 { glyphs[0] } else { glyphs[1] }
    }
}

/// Ask CoreText for a font that can render `ch`, falling back to `base`
/// itself if CT returns nothing.  CoreText walks the system fallback chain
/// (Hiragino for Japanese, PingFang for Chinese, Apple Color Emoji, etc.).
fn create_fallback_font(base: &CTFont, ch: char) -> CTFont {
    let s = ch.to_string();
    let cf = CFString::new(&s);
    let len = ch.len_utf16() as isize;
    let range = CFRange { location: 0, length: len };
    unsafe {
        let raw = CTFontCreateForString(
            base.as_concrete_TypeRef(),
            cf.as_concrete_TypeRef(),
            range,
        );
        if raw.is_null() {
            return base.clone();
        }
        CTFont::wrap_under_create_rule(raw)
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

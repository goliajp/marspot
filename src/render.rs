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

use crate::grid::{Cell, CellAttrs, Color, Grid};
use crate::layout::{CellRect, Layout};
use crate::session::SessionState;
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
use core_text::font_descriptor::{
    kCTFontBoldTrait, kCTFontItalicTrait, kCTFontOrientationDefault,
};
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSColor, NSImage, NSView};
use objc2_foundation::NSSize;
use objc2_quartz_core::{kCAGravityTopLeft, CALayer};
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
/// Sidebar background — slightly different shade so it reads as
/// chrome separate from terminal cells.
const SIDEBAR_BG: (CGFloat, CGFloat, CGFloat) = (0.08, 0.10, 0.14);
/// Thin gutter between session cells when more than one is on screen.
const GUTTER: (CGFloat, CGFloat, CGFloat) = (0.02, 0.03, 0.06);
/// Outline drawn around the focused session cell.
const FOCUS_OUTLINE: (CGFloat, CGFloat, CGFloat) = (0.30, 0.55, 0.95);

/// Per-session render parameters.  Caller bundles the relevant bits
/// so the renderer doesn't need to know about Session, Mars, or
/// MarsEvent — anything that can produce a Grid + view offset can
/// drive a render.
pub struct SessionView<'a> {
    pub grid: &'a Grid,
    /// Lines scrolled up from live (0 = live view).
    pub view_offset: u16,
    /// DECTCEM (?25) — false ⇒ hide cursor for this session.
    pub cursor_visible: bool,
    /// True for the session that owns the keyboard right now.
    /// Drives the focus outline + cursor style (filled vs hollow).
    pub focused: bool,
}

/// One row of the sidebar — what the user sees on the left.  Length
/// of the slice passed to [`Renderer::render_layout`] should match
/// `views.len()`; entry `i` describes session `i`.
pub struct SidebarEntry<'a> {
    /// Short label drawn after the state dot, e.g. `"1"`, `"build"`.
    pub label: &'a str,
    /// Drives the colour of the state dot.
    pub state: SessionState,
}

/// Sidebar typography (in points-equivalent pixels at scale = 1).
/// Tuned for the default 200-pt-wide sidebar.
const SIDEBAR_DOT_R: f64 = 4.5;
const SIDEBAR_LEFT_PAD: f64 = 14.0;
const SIDEBAR_TOP_PAD: f64 = 14.0;
const SIDEBAR_ROW_H: f64 = 22.0;
/// Gap between the dot's right edge and the start of the label text.
const SIDEBAR_DOT_LABEL_GAP: f64 = 10.0;
const SIDEBAR_TEXT_FG: (CGFloat, CGFloat, CGFloat) = (0.78, 0.82, 0.88);
const SIDEBAR_FOCUSED_BG: (CGFloat, CGFloat, CGFloat) = (0.13, 0.18, 0.30);
const STATE_ACTIVE: (CGFloat, CGFloat, CGFloat) = (0.30, 0.85, 0.45);
const STATE_IDLE: (CGFloat, CGFloat, CGFloat) = (0.55, 0.58, 0.62);
const STATE_EXITED: (CGFloat, CGFloat, CGFloat) = (0.85, 0.30, 0.30);

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
    /// Per-(codepoint, style) resolution: which font (by index into
    /// `fonts`) and glyph id can render this codepoint with the given
    /// bold/italic combination.  Filled lazily.
    ///
    /// Style is a 2-bit packed value: bit 0 = bold, bit 1 = italic, so
    ///   0 = regular, 1 = bold, 2 = italic, 3 = bold-italic
    char_cache: HashMap<(u32, u8), (usize, CGGlyph)>,
    /// Indices into `fonts` for the four base styles of the primary
    /// font, so styled cells can pick a variant in O(1).  When a
    /// variant doesn't exist (e.g. Menlo lacks a true italic), this
    /// falls back to the regular font index.
    style_font_idx: [usize; 4],
    cell_w: f64,
    cell_h: f64,
    ascent: f64,
    viewport_w: f64,
    viewport_h: f64,
    scale: f64,
    /// Window-level focus.  When false, even the focused-session
    /// cursor draws hollow because the user clearly isn't typing
    /// into mars.
    window_focused: bool,
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

        // Pre-compute the four style variants of the base font.  Some
        // fonts lack a true italic — fall back to regular for any miss
        // so SGR italic still renders something rather than panicking.
        let bold_mask = kCTFontBoldTrait;
        let italic_mask = kCTFontItalicTrait;
        let try_variant =
            |traits: u32| font.clone_with_symbolic_traits(traits, bold_mask | italic_mask);
        let regular = font.clone();
        let bold = try_variant(bold_mask).unwrap_or_else(|| font.clone());
        let italic = try_variant(italic_mask).unwrap_or_else(|| font.clone());
        let bold_italic =
            try_variant(bold_mask | italic_mask).unwrap_or_else(|| font.clone());

        let layer = if let Some(view) = view {
            view.setWantsLayer(true);
            // The view is now layer-backed; AppKit creates a default
            // CALayer for us.  We grab it and set its contentsScale so
            // CA composites at the right density.
            let layer = unsafe {
                view.layer()
                    .ok_or("layer not available on view".to_string())?
            };
            unsafe {
                layer.setContentsScale(scale as f64);
                // Default contentsGravity is `resize` — during a live
                // resize the previous frame's CGImage gets stretched to
                // the new layer bounds before our redraw lands, which
                // looks like the BG flickering / changing colour.  Pin
                // the existing image to the top-left and let the BG show
                // through the gap until the next frame is ready.
                layer.setContentsGravity(kCAGravityTopLeft);
                // The terminal is fully opaque; tell CA so it can skip
                // compositing anything underneath.
                layer.setOpaque(true);
            }
            // Make the NSWindow's background match the terminal BG so
            // the gap that briefly shows during live resize (before our
            // newly-sized CGImage arrives on the layer) is the same
            // colour as the terminal — visually no flicker.
            unsafe {
                if let Some(window) = view.window() {
                    let bg = NSColor::colorWithSRGBRed_green_blue_alpha(
                        BG.0, BG.1, BG.2, 1.0,
                    );
                    window.setBackgroundColor(Some(&bg));
                }
            }
            Some(layer)
        } else {
            None
        };

        let mut fonts = FontRegistry::new(regular);
        let bold_idx = fonts.intern(bold);
        let italic_idx = fonts.intern(italic);
        let bold_italic_idx = fonts.intern(bold_italic);

        Ok(Self {
            layer,
            fonts,
            char_cache: HashMap::new(),
            style_font_idx: [0, bold_idx, italic_idx, bold_italic_idx],
            cell_w,
            cell_h,
            ascent,
            viewport_w: 0.0,
            viewport_h: 0.0,
            scale: scale as f64,
            window_focused: true,
        })
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// Returns the cell to render at the given viewport position
    /// inside one session, honouring its `view_offset`.  Pulls from
    /// `grid` for live rows and from `grid.scrollback_line(_)` for
    /// scrolled-up rows.  Returns a blank cell for positions past
    /// the oldest scrollback line.
    fn cell_at_viewport(view_offset: u16, col: u16, viewport_row: u16, grid: &Grid) -> Cell {
        let rows = grid.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            grid.cell(col, (rows - 1 - abs) as u16)
        } else {
            let from_end = abs - rows;
            let sb_len = grid.scrollback_len();
            if from_end < sb_len {
                let sb_idx = sb_len - 1 - from_end;
                if let Some(line) = grid.scrollback_line(sb_idx) {
                    if (col as usize) < line.len() {
                        return line[col as usize];
                    }
                }
            }
            Cell::default()
        }
    }

    /// Resolve a character + style to (font_idx, glyph).  Tries the
    /// requested style first; on .notdef asks CoreText for a per-string
    /// fallback (which loses the style — we don't try to bold/italic
    /// fallback fonts).  Cached by (codepoint, style).
    fn resolve_char(&mut self, ch: char, bold: bool, italic: bool) -> (usize, CGGlyph) {
        let style: u8 = (bold as u8) | ((italic as u8) << 1);
        let key = (ch as u32, style);
        if let Some(&entry) = self.char_cache.get(&key) {
            return entry;
        }
        let style_idx = self.style_font_idx[style as usize];
        let base = self.fonts.fonts[style_idx].clone();
        let glyph = lookup_glyph(&base, ch);
        let entry = if glyph != 0 {
            (style_idx, glyph)
        } else {
            // Fallback path: ignore style.  CoreText's per-string fallback
            // gives us a CJK / emoji font that won't have its own bold or
            // italic anyway.
            let fallback = create_fallback_font(&base, ch);
            let fb_glyph = lookup_glyph(&fallback, ch);
            let idx = self.fonts.intern(fallback);
            (idx, fb_glyph)
        };
        self.char_cache.insert(key, entry);
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

    /// Render a single session full-window (mcli + the snapshot bench).
    /// Convenience wrapper over [`render_layout`](Self::render_layout)
    /// using a 1-cell layout that fills the viewport.
    pub fn render(&mut self, view: SessionView) {
        if self.layer.is_none() {
            return;
        }
        if self.viewport_w < 1.0 || self.viewport_h < 1.0 {
            return;
        }
        let layout = Layout::build(
            self.viewport_w,
            self.viewport_h,
            0.0,
            1,
            1,
            self.cell_w,
            self.cell_h,
        );
        self.render_layout(&layout, std::slice::from_ref(&view), &[], 0);
    }

    /// Render N session views + a sidebar into a window-sized frame.
    /// Each view's rect is taken from `layout.cells[i]`; cells beyond
    /// `views.len()` stay as the default frame background (so a
    /// partially-filled grid reads as empty cells).  When `sidebar`
    /// is empty no chrome text is drawn (mcli passes `&[]`).
    pub fn render_layout(
        &mut self,
        layout: &Layout,
        views: &[SessionView],
        sidebar: &[SidebarEntry],
        focused_idx: usize,
    ) {
        if self.layer.is_none() {
            return;
        }
        if self.viewport_w < 1.0 || self.viewport_h < 1.0 {
            return;
        }
        let total_w = self.viewport_w as u32;
        let total_h = self.viewport_h as u32;
        let ctx = self.frame_context(total_w, total_h, layout);
        for (i, view) in views.iter().enumerate() {
            if let Some(rect) = layout.cells.get(i) {
                self.draw_session_in_rect(&ctx, total_h, rect, view);
            }
        }
        if !sidebar.is_empty() && layout.sidebar_w > 0.0 {
            self.draw_sidebar(&ctx, total_h, layout, sidebar, focused_idx);
        }
        let cgimage = ctx
            .create_image()
            .expect("CGContext should produce a CGImage");
        let layer = self.layer.as_ref().unwrap();
        let cg_ptr = cgimage.as_ptr() as *const AnyObject;
        unsafe {
            layer.setContents(Some(&*cg_ptr));
        }
    }

    /// Headless-friendly variant: render a single session into a
    /// freshly-allocated bitmap context at `width × height` physical
    /// pixels and return the raw bytes (PNG wants RGBA; we swap
    /// R↔B per pixel to honour that).
    pub fn snapshot(
        &mut self,
        width: u32,
        height: u32,
        grid: &Grid,
    ) -> Result<Vec<u8>, String> {
        self.viewport_w = width as f64;
        self.viewport_h = height as f64;
        let layout = Layout::build(
            width as f64,
            height as f64,
            0.0,
            1,
            1,
            self.cell_w,
            self.cell_h,
        );
        let view = SessionView {
            grid,
            view_offset: 0,
            cursor_visible: true,
            focused: true,
        };
        let mut ctx = self.frame_context(width, height, &layout);
        if let Some(rect) = layout.cells.first() {
            self.draw_session_in_rect(&ctx, height, rect, &view);
        }
        let mut bytes = ctx.data().to_vec();
        for chunk in bytes.chunks_exact_mut(4) {
            chunk.swap(0, 2);
        }
        Ok(bytes)
    }

    /// Build a full-window CGBitmapContext, fill it with the chrome
    /// background (sidebar + gutter colour), so per-session cells only
    /// have to fill their own backgrounds — anything they don't paint
    /// reads as chrome.
    fn frame_context(&self, width: u32, height: u32, layout: &Layout) -> CGContext {
        let space = CGColorSpace::create_device_rgb();
        let row_bytes = width as usize * 4;
        let bitmap_info = kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big;
        let ctx = CGContext::create_bitmap_context(
            None,
            width as usize,
            height as usize,
            8,
            row_bytes,
            &space,
            bitmap_info,
        );

        // Chrome background: GUTTER colour everywhere.  The sidebar
        // gets its own slightly different fill on top.
        ctx.set_rgb_fill_color(GUTTER.0, GUTTER.1, GUTTER.2, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(width as f64, height as f64),
        ));
        if layout.sidebar_w > 0.0 {
            ctx.set_rgb_fill_color(SIDEBAR_BG.0, SIDEBAR_BG.1, SIDEBAR_BG.2, 1.0);
            ctx.fill_rect(CGRect::new(
                &CGPoint::new(0.0, 0.0),
                &CGSize::new(layout.sidebar_w, height as f64),
            ));
        }
        ctx
    }

    /// Render `view` into `rect` — fills the rect's terminal background,
    /// draws BG cells / glyphs / underlines / cursor, and (for the
    /// focused session) outlines the cell with a thin focus ring.
    fn draw_session_in_rect(
        &mut self,
        ctx: &CGContext,
        total_h: u32,
        rect: &CellRect,
        view: &SessionView,
    ) {
        // Top of the rect in CG's y-up coords (y=0 is the bottom of
        // the bitmap context).  All per-row baselines are computed
        // off this anchor.
        let rect_top_y_up = total_h as f64 - rect.y_top;

        // Terminal-cell background fill for this rect.  Sits on top
        // of frame_context's chrome fill, so the rect now reads as a
        // proper terminal cell.
        ctx.set_rgb_fill_color(BG.0, BG.1, BG.2, 1.0);
        ctx.fill_rect(CGRect::new(
            &CGPoint::new(rect.x, rect_top_y_up - rect.h),
            &CGSize::new(rect.w, rect.h),
        ));

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

        let grid = view.grid;
        let cols = grid.cols() as usize;
        let mut run_glyphs: Vec<CGGlyph> = Vec::with_capacity(cols);
        let mut run_positions: Vec<CGPoint> = Vec::with_capacity(cols);

        for r in 0..grid.rows() {
            // 1) Background pass — fill runs of cells that share a non-default
            //    background color.  Cells with the default BG inherit the
            //    rect's terminal-bg fill we just laid down.
            let row_bottom_y = rect_top_y_up - (r as f64 + 1.0) * self.cell_h;
            let mut c = 0usize;
            while c < cols {
                let bg = resolve_attrs(
                    Self::cell_at_viewport(view.view_offset, c as u16, r, grid).attrs,
                )
                .1;
                if bg == BG {
                    c += 1;
                    continue;
                }
                let start = c;
                c += 1;
                while c < cols
                    && resolve_attrs(
                        Self::cell_at_viewport(view.view_offset, c as u16, r, grid).attrs,
                    )
                    .1 == bg
                {
                    c += 1;
                }
                ctx.set_rgb_fill_color(bg.0, bg.1, bg.2, 1.0);
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(rect.x + start as f64 * self.cell_w, row_bottom_y),
                    &CGSize::new((c - start) as f64 * self.cell_w, self.cell_h),
                ));
            }

            // 2) Foreground pass — group consecutive non-blank cells that
            //    share both font and fg color; one draw_glyphs call per run.
            let baseline_y = rect_top_y_up - (r as f64 * self.cell_h + self.ascent);
            let mut i = 0usize;
            while i < cols {
                let cell = Self::cell_at_viewport(view.view_offset, i as u16, r, grid);
                if cell.ch == ' ' || cell.ch == '\0' {
                    i += 1;
                    continue;
                }
                let (font_idx, glyph) =
                    self.resolve_char(cell.ch, cell.attrs.bold, cell.attrs.italic);
                let fg = resolve_attrs(cell.attrs).0;
                run_glyphs.clear();
                run_positions.clear();
                run_glyphs.push(glyph);
                run_positions.push(CGPoint::new(rect.x + i as f64 * self.cell_w, baseline_y));
                i += 1;
                while i < cols {
                    let cur = Self::cell_at_viewport(view.view_offset, i as u16, r, grid);
                    if cur.ch == ' ' || cur.ch == '\0' {
                        break;
                    }
                    let (cur_font, cur_glyph) =
                        self.resolve_char(cur.ch, cur.attrs.bold, cur.attrs.italic);
                    if cur_font != font_idx || resolve_attrs(cur.attrs).0 != fg {
                        break;
                    }
                    run_glyphs.push(cur_glyph);
                    run_positions.push(CGPoint::new(rect.x + i as f64 * self.cell_w, baseline_y));
                    i += 1;
                }
                ctx.set_rgb_fill_color(fg.0, fg.1, fg.2, 1.0);
                let font = &self.fonts.fonts[font_idx];
                font.draw_glyphs(&run_glyphs, &run_positions, ctx.clone());
            }

            // 3) Underline pass.
            let underline_y = row_bottom_y + (self.cell_h - self.ascent) * 0.55;
            let underline_h = (self.cell_h * 0.06).max(1.0);
            let mut u = 0usize;
            while u < cols {
                let cell = Self::cell_at_viewport(view.view_offset, u as u16, r, grid);
                if !cell.attrs.underline {
                    u += 1;
                    continue;
                }
                let fg = resolve_attrs(cell.attrs).0;
                let start = u;
                u += 1;
                while u < cols {
                    let cur = Self::cell_at_viewport(view.view_offset, u as u16, r, grid);
                    if !cur.attrs.underline || resolve_attrs(cur.attrs).0 != fg {
                        break;
                    }
                    u += 1;
                }
                ctx.set_rgb_fill_color(fg.0, fg.1, fg.2, 1.0);
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(rect.x + start as f64 * self.cell_w, underline_y),
                    &CGSize::new((u - start) as f64 * self.cell_w, underline_h),
                ));
            }
        }

        // Cursor: only when this session is in live view + DECTCEM is on.
        if view.view_offset == 0 && view.cursor_visible {
            self.draw_cursor_in_rect(ctx, rect_top_y_up, rect, view);
        }

        // Focus outline: a thin border around the focused cell.  Helps
        // distinguish "the one currently receiving keystrokes" from the
        // others when more than one session is on screen.
        if view.focused {
            let stroke = (self.cell_h * 0.10).max(1.0);
            ctx.set_rgb_stroke_color(
                FOCUS_OUTLINE.0,
                FOCUS_OUTLINE.1,
                FOCUS_OUTLINE.2,
                1.0,
            );
            ctx.set_line_width(stroke);
            ctx.stroke_rect(CGRect::new(
                &CGPoint::new(rect.x, rect_top_y_up - rect.h),
                &CGSize::new(rect.w, rect.h),
            ));
        }
    }

    /// Sidebar pass: one row per session.  Row N: [focus highlight bg,]
    /// state dot, label.  Caller is responsible for matching the row
    /// height & top padding constants when hit-testing clicks
    /// (`SIDEBAR_TOP_PAD`, `SIDEBAR_ROW_H`).
    fn draw_sidebar(
        &mut self,
        ctx: &CGContext,
        total_h: u32,
        layout: &Layout,
        entries: &[SidebarEntry],
        focused_idx: usize,
    ) {
        for (i, entry) in entries.iter().enumerate() {
            let row_top_y_down = SIDEBAR_TOP_PAD + i as f64 * SIDEBAR_ROW_H;
            let row_top_y_up = total_h as f64 - row_top_y_down;
            let row_bottom_y = row_top_y_up - SIDEBAR_ROW_H;

            // Focus highlight: subtle blue-grey fill behind the focused row.
            if i == focused_idx {
                ctx.set_rgb_fill_color(
                    SIDEBAR_FOCUSED_BG.0,
                    SIDEBAR_FOCUSED_BG.1,
                    SIDEBAR_FOCUSED_BG.2,
                    1.0,
                );
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(0.0, row_bottom_y),
                    &CGSize::new(layout.sidebar_w, SIDEBAR_ROW_H),
                ));
            }

            // State dot.  Vertically-centred in the row.
            let dot_color = match entry.state {
                SessionState::Active => STATE_ACTIVE,
                SessionState::Idle => STATE_IDLE,
                SessionState::Exited => STATE_EXITED,
            };
            let dot_cx = SIDEBAR_LEFT_PAD + SIDEBAR_DOT_R;
            let dot_cy = row_top_y_up - SIDEBAR_ROW_H / 2.0;
            ctx.set_rgb_fill_color(dot_color.0, dot_color.1, dot_color.2, 1.0);
            ctx.fill_ellipse_in_rect(CGRect::new(
                &CGPoint::new(dot_cx - SIDEBAR_DOT_R, dot_cy - SIDEBAR_DOT_R),
                &CGSize::new(SIDEBAR_DOT_R * 2.0, SIDEBAR_DOT_R * 2.0),
            ));

            // Label text.  Reuse the base font; lay out via cell_w
            // monospace metrics — sidebar chars are typically 1–8
            // ASCII so cell_w accuracy is fine.
            let label_x = dot_cx + SIDEBAR_DOT_R + SIDEBAR_DOT_LABEL_GAP;
            let baseline_y = row_top_y_up - SIDEBAR_ROW_H / 2.0
                + self.ascent * 0.40 - self.cell_h * 0.20;
            ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
            ctx.set_rgb_fill_color(
                SIDEBAR_TEXT_FG.0,
                SIDEBAR_TEXT_FG.1,
                SIDEBAR_TEXT_FG.2,
                1.0,
            );
            let mut glyphs: Vec<CGGlyph> = Vec::with_capacity(entry.label.len());
            let mut positions: Vec<CGPoint> = Vec::with_capacity(entry.label.len());
            let mut x = label_x;
            for ch in entry.label.chars() {
                let (font_idx, g) = self.resolve_char(ch, false, false);
                if g != 0 && font_idx == 0 {
                    glyphs.push(g);
                    positions.push(CGPoint::new(x, baseline_y));
                }
                x += self.cell_w;
            }
            if !glyphs.is_empty() {
                let font = &self.fonts.fonts[0];
                font.draw_glyphs(&glyphs, &positions, ctx.clone());
            }
        }
    }

    /// Cursor: filled block when this session is focused AND the window
    /// is focused; hollow outline otherwise.  For the filled case we
    /// re-draw the cell's glyph in the background colour so the
    /// character stays readable.
    fn draw_cursor_in_rect(
        &mut self,
        ctx: &CGContext,
        rect_top_y_up: f64,
        rect: &CellRect,
        view: &SessionView,
    ) {
        let (col, row) = view.grid.cursor();
        let cx = rect.x + col as f64 * self.cell_w;
        let cy_bottom = rect_top_y_up - (row as f64 + 1.0) * self.cell_h;
        let cursor_rect = CGRect::new(
            &CGPoint::new(cx, cy_bottom),
            &CGSize::new(self.cell_w, self.cell_h),
        );
        let solid = view.focused && self.window_focused;

        if solid {
            ctx.set_rgb_fill_color(FG.0, FG.1, FG.2, 1.0);
            ctx.fill_rect(cursor_rect);

            let cell = view.grid.cell(col, row);
            if cell.ch != ' ' && cell.ch != '\0' {
                let (font_idx, glyph) =
                    self.resolve_char(cell.ch, cell.attrs.bold, cell.attrs.italic);
                if glyph != 0 {
                    ctx.set_rgb_fill_color(BG.0, BG.1, BG.2, 1.0);
                    let baseline_y =
                        rect_top_y_up - (row as f64 * self.cell_h + self.ascent);
                    let font = &self.fonts.fonts[font_idx];
                    font.draw_glyphs(
                        &[glyph],
                        &[CGPoint::new(cx, baseline_y)],
                        ctx.clone(),
                    );
                }
            }
        } else {
            // Hollow outline.
            let stroke = (self.cell_h * 0.07).max(1.0);
            ctx.set_rgb_stroke_color(FG.0, FG.1, FG.2, 1.0);
            ctx.set_line_width(stroke);
            ctx.stroke_rect(cursor_rect);
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

//! Font + colour resolution shared across renderers.
//!
//! Both `render.rs` (AppKit/CGImage path) and `render_metal.rs` (CAMetalLayer
//! path) need:
//!   * a base font + bold / italic / bold-italic variants
//!   * a fallback font registry for codepoints the base font lacks
//!   * a `(codepoint, style) → (font_idx, glyph)` cache
//!   * the standard ANSI palette + SGR-reverse handling
//!
//! Keeping that one canonical implementation here means the Metal path
//! never drifts from the AppKit path's choices around hinting,
//! fallback selection, or palette values — important during the A/B
//! integration phase where pixels need to match.

use crate::grid::{CellAttrs, Color};
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::base::CGFloat;
use core_graphics::font::CGGlyph;
use core_graphics::geometry::CGSize;
use core_text::font::{new_from_name, CTFont, CTFontRef};
use core_text::font_descriptor::{
    kCTFontBoldTrait, kCTFontItalicTrait, kCTFontOrientationDefault,
};
use std::collections::HashMap;

// Match iTerm2's default profile (Normal Font = "Monaco 12") so a
// user switching from iTerm2 to mars sees identical text.  Menlo is
// retained as the fallback chain target inside FontCache::build for
// the rare case where Monaco isn't installed (it ships with macOS,
// so this should never fire in practice).
pub const FONT_NAME: &str = "Monaco";
pub const FONT_POINT: f64 = 12.0;

/// Background color for the terminal.  Near-pure-black with a
/// near-imperceptible navy tint — the user's preferred direction
/// after seeing iTerm2's #14191e default felt too grey in mars's
/// 9-grid layout.  Both renderers paint with this constant so the
/// BG matches across the AppKit / Metal switch.
pub const BG: (CGFloat, CGFloat, CGFloat) = (0.006, 0.008, 0.014);
/// Default foreground.  Matches iTerm2's "Foreground Color (Dark)"
/// — slightly off-white (`#dbdbdb`), softer than pure 0.92 grey on
/// the eyes for long-running sessions.
pub const FG: (CGFloat, CGFloat, CGFloat) = (0.8620, 0.8620, 0.8620);

/// ANSI 16-colour palette — copied from iTerm2's default (Dark)
/// profile so SGR 30..37 / 90..97 colours render identically to
/// what the user is used to seeing in iTerm2.  Values via
/// `defaults read com.googlecode.iterm2 "New Bookmarks"`,
/// "Ansi N Color (Dark)" entries.
pub const ANSI_16: [(CGFloat, CGFloat, CGFloat); 16] = [
    (0.0784, 0.0980, 0.1176), //  0 black           #14191e
    (0.7074, 0.2366, 0.1630), //  1 red             #b43c29
    (0.0000, 0.7608, 0.0000), //  2 green           #00c200
    (0.7806, 0.7696, 0.0000), //  3 yellow          #c7c400
    (0.1540, 0.2647, 0.7822), //  4 blue            #2743c7
    (0.7522, 0.2493, 0.7449), //  5 magenta         #bf3fbd
    (0.0000, 0.7743, 0.7817), //  6 cyan            #00c5c7
    (0.7810, 0.7811, 0.7810), //  7 white           #c7c7c7
    (0.4078, 0.4078, 0.4078), //  8 bright black    #676767
    (0.8660, 0.4752, 0.4583), //  9 bright red      #dc7974
    (0.3450, 0.9043, 0.5654), // 10 bright green    #57e690
    (0.9259, 0.8834, 0.0000), // 11 bright yellow   #ece100
    (0.6535, 0.6704, 0.9485), // 12 bright blue     #a6aaf1
    (0.8822, 0.4927, 0.8822), // 13 bright magenta  #e07de0
    (0.3760, 0.9926, 1.0000), // 14 bright cyan     #5ffdff
    (1.0000, 1.0000, 1.0000), // 15 bright white    #feffff
];

pub fn palette_color(idx: u8) -> (CGFloat, CGFloat, CGFloat) {
    if (idx as usize) < ANSI_16.len() {
        return ANSI_16[idx as usize];
    }
    if idx < 232 {
        const RAMP: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let n = idx - 16;
        let r = RAMP[(n / 36) as usize];
        let g = RAMP[((n / 6) % 6) as usize];
        let b = RAMP[(n % 6) as usize];
        return (
            r as f64 / 255.0,
            g as f64 / 255.0,
            b as f64 / 255.0,
        );
    }
    let v = 8 + (idx - 232) as i32 * 10;
    let f = v as f64 / 255.0;
    (f, f, f)
}

pub fn resolve_color(
    c: Color,
    default_rgb: (CGFloat, CGFloat, CGFloat),
) -> (CGFloat, CGFloat, CGFloat) {
    match c {
        Color::Default => default_rgb,
        Color::Indexed(i) => palette_color(i),
        Color::Rgb(r, g, b) => (
            r as f64 / 255.0,
            g as f64 / 255.0,
            b as f64 / 255.0,
        ),
    }
}

/// Resolve a cell's attrs to (fg, bg) RGB, honouring SGR reverse.
pub fn resolve_attrs(
    attrs: CellAttrs,
) -> (
    (CGFloat, CGFloat, CGFloat),
    (CGFloat, CGFloat, CGFloat),
) {
    let mut fg = resolve_color(attrs.fg, FG);
    let mut bg = resolve_color(attrs.bg, BG);
    if attrs.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

/// CoreText's per-string font fallback resolver — not in the
/// `core-text` crate so we declare it directly.
#[link(name = "CoreText", kind = "framework")]
extern "C" {
    fn CTFontCreateForString(
        currentFont: CTFontRef,
        string: CFStringRef,
        range: core_foundation::base::CFRange,
    ) -> CTFontRef;
}

/// Holds the base font + lazily-discovered fallbacks.  Postscript
/// name → index map dedups instances (CT hands out a fresh CTFontRef
/// each lookup even when the underlying font is the same).
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

/// Shared font handling for both renderers.  Owns the font registry,
/// the glyph cache, and the precomputed cell metrics.
pub struct FontCache {
    fonts: FontRegistry,
    /// `(codepoint, style)` → `(font_idx, glyph)`.  Style is 2-bit:
    /// bit 0 = bold, bit 1 = italic.  Bounded at `CHAR_CACHE_CAP`;
    /// atomic-rebuild on full (drop everything, re-resolve on next
    /// access) — same pattern as `GlyphAtlas`.  Realistic terminal
    /// use exposes a few hundred to a few thousand unique
    /// codepoints across the 4 styles, well under cap; the rebuild
    /// is the safety net for pathological "user types every
    /// codepoint of CJK + emoji" cases.
    char_cache: HashMap<(u32, u8), (usize, CGGlyph)>,
    /// Number of times the cache filled up and was rebuilt.
    /// Should be 0 in steady-state terminal use; non-zero after
    /// settling means we're hitting the cap (bump CHAR_CACHE_CAP
    /// or move to a real LRU).
    pub rebuild_count: u64,
    /// Index into `fonts` for each of the 4 base styles (regular,
    /// bold, italic, bold-italic).  Falls back to regular when a
    /// variant doesn't exist (e.g. Menlo lacks true italic).
    style_font_idx: [usize; 4],
    pub cell_w: f64,
    pub cell_h: f64,
    pub ascent: f64,
}

/// Hard cap on `char_cache` entries.  Realistic terminal use
/// across 9 simultaneous sessions is well under 4 K unique
/// codepoints × 4 styles = 16 K entries; 8 K cap holds a typical
/// working set with headroom and rebuilds rarely.  Each entry is
/// ~50–70 B with HashMap accounting, so 8 K cap = ~500 KiB resident
/// — bounded for the lifetime of the process.
const CHAR_CACHE_CAP: usize = 8192;

impl FontCache {
    pub fn build() -> Result<Self, String> {
        let font = new_from_name(FONT_NAME, FONT_POINT)
            .or_else(|_| new_from_name("Menlo", FONT_POINT))
            .map_err(|_| "could not load font".to_string())?;

        let cell_w = compute_cell_width(&font);
        let ascent = font.ascent();
        let cell_h = ascent + font.descent() + font.leading();

        let bold_mask = kCTFontBoldTrait;
        let italic_mask = kCTFontItalicTrait;
        let try_variant =
            |traits: u32| font.clone_with_symbolic_traits(traits, bold_mask | italic_mask);
        let regular = font.clone();
        let bold = try_variant(bold_mask).unwrap_or_else(|| font.clone());
        let italic = try_variant(italic_mask).unwrap_or_else(|| font.clone());
        let bold_italic =
            try_variant(bold_mask | italic_mask).unwrap_or_else(|| font.clone());

        let mut fonts = FontRegistry::new(regular);
        let bold_idx = fonts.intern(bold);
        let italic_idx = fonts.intern(italic);
        let bold_italic_idx = fonts.intern(bold_italic);

        Ok(Self {
            fonts,
            char_cache: HashMap::new(),
            rebuild_count: 0,
            style_font_idx: [0, bold_idx, italic_idx, bold_italic_idx],
            cell_w,
            cell_h,
            ascent,
        })
    }

    /// Resolve a `(char, bold, italic)` triple to `(font_idx, glyph)`.
    /// Tries the requested style first; on `.notdef` falls back to a
    /// per-string CT-discovered font (which loses the style — fallback
    /// fonts rarely have their own bold/italic anyway).  Cached.
    pub fn resolve_char(&mut self, ch: char, bold: bool, italic: bool) -> (usize, CGGlyph) {
        let style: u8 = (bold as u8) | ((italic as u8) << 1);
        let key = (ch as u32, style);
        if let Some(&entry) = self.char_cache.get(&key) {
            return entry;
        }
        // Atomic rebuild when full.  Realistic terminal working sets
        // stay well under cap, so this is a safety net rather than
        // a frequent path; if rebuild_count climbs in the wild,
        // raise the cap or upgrade to true LRU.  Note we don't drop
        // the FontRegistry — fallback fonts already discovered stay
        // interned (they're heavy to recreate via CT discover).
        if self.char_cache.len() >= CHAR_CACHE_CAP {
            self.char_cache.clear();
            self.rebuild_count += 1;
        }
        let style_idx = self.style_font_idx[style as usize];
        let base = self.fonts.fonts[style_idx].clone();
        let glyph = lookup_glyph(&base, ch);
        let entry = if glyph != 0 {
            (style_idx, glyph)
        } else {
            let fallback = create_fallback_font(&base, ch);
            let fb_glyph = lookup_glyph(&fallback, ch);
            let idx = self.fonts.intern(fallback);
            (idx, fb_glyph)
        };
        self.char_cache.insert(key, entry);
        entry
    }

    pub fn font(&self, idx: usize) -> &CTFont {
        &self.fonts.fonts[idx]
    }

    pub fn cell_dims(&self) -> (f64, f64) {
        (self.cell_w, self.cell_h)
    }
}

pub fn lookup_glyph(font: &CTFont, ch: char) -> CGGlyph {
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
        // Apple's docs disagree across versions about lead vs trail
        // surrogate; pick the non-zero one to avoid .notdef.
        if glyphs[0] != 0 {
            glyphs[0]
        } else {
            glyphs[1]
        }
    }
}

pub fn create_fallback_font(base: &CTFont, ch: char) -> CTFont {
    let s = ch.to_string();
    let cf = CFString::new(&s);
    let len = ch.len_utf16() as isize;
    let range = core_foundation::base::CFRange {
        location: 0,
        length: len,
    };
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

pub fn compute_cell_width(font: &CTFont) -> f64 {
    let mut glyph: CGGlyph = 0;
    let m: u16 = b'M' as u16;
    let _ok = unsafe { font.get_glyphs_for_characters(&m, &mut glyph, 1) };
    if glyph == 0 {
        return 7.0;
    }
    let mut size = CGSize::new(0.0, 0.0);
    unsafe {
        font.get_advances_for_glyphs(kCTFontOrientationDefault, &glyph, &mut size, 1);
    }
    size.width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_cache_atomic_rebuild_on_cap() {
        let mut fc = match FontCache::build() {
            Ok(f) => f,
            Err(_) => return, // CI without the system font
        };

        // Push CHAR_CACHE_CAP unique (codepoint, style) keys through.
        // ASCII printable space is 95 chars × 4 styles = 380 keys per
        // round; iterate enough rounds with synthetic codepoints to
        // exceed cap.
        let mut pushed = 0;
        for cp in 0x20u32..0x20u32 + (CHAR_CACHE_CAP as u32 + 100) {
            if let Some(ch) = char::from_u32(cp) {
                fc.resolve_char(ch, false, false);
                pushed += 1;
            }
        }
        assert!(pushed > CHAR_CACHE_CAP, "pushed enough to overflow");
        assert!(
            fc.rebuild_count > 0,
            "cap should have triggered at least one rebuild"
        );
        // After rebuild, cache len is bounded.
        assert!(
            fc.char_cache.len() <= CHAR_CACHE_CAP,
            "cache must respect cap"
        );
        // Re-resolving should still work — atomic rebuild doesn't
        // break the public contract.
        let (_idx, glyph) = fc.resolve_char('A', false, false);
        assert!(glyph != 0, "resolve still works post-rebuild");
    }
}

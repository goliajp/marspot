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
    kCTFontBoldTrait, kCTFontColorGlyphsTrait, kCTFontItalicTrait, kCTFontOrientationDefault,
};
use std::collections::HashMap;

// Match iTerm2's default profile (Normal Font = "Monaco 12") so a
// user switching from iTerm2 to marspot sees identical text.  Menlo is
// retained as the fallback chain target inside FontCache::build for
// the rare case where Monaco isn't installed (it ships with macOS,
// so this should never fire in practice).
// Font choice: Monaco — macOS-native, what every old-school terminal
// app reaches for.  Was briefly switched to JetBrains Mono when the
// CT font-smoothing thickening + sRGB-encoded Metal target were
// double-bolding glyphs, but with both fixed (atlas no longer asks
// for smoothing, target is `BGRA8Unorm`) Monaco reads at the
// designed weight again — the comparison-to-iTerm2 sweet spot.
pub const FONT_NAME: &str = "Monaco";
pub const FONT_POINT: f64 = 12.0;

/// Background color for the terminal.  Near-pure-black with a
/// near-imperceptible navy tint — the user's preferred direction
/// after seeing iTerm2's #14191e default felt too grey in marspot's
/// 9-grid layout.  Both renderers paint with this constant so the
/// BG matches across the AppKit / Metal switch.
pub const BG: (CGFloat, CGFloat, CGFloat) = (0.006, 0.008, 0.014);
/// Default foreground.  Matches iTerm2's "Foreground Color (Dark)"
/// — slightly off-white (`#dbdbdb`), softer than pure 0.92 grey on
/// the eyes for long-running sessions.
pub const FG: (CGFloat, CGFloat, CGFloat) = (0.8620, 0.8620, 0.8620);

/// ANSI 16-colour palette — punchier than iTerm2's stock Dark.  iTerm2
/// Dark's bright-red (#dc7974) and bright-magenta (#e07de0) lean
/// pink-salmon, and bright-blue (#a6aaf1) is lavender; the user
/// flagged these as "red looks pink, everything looks grey".  This
/// table keeps the dark variants similar (they're already grounded)
/// but bumps the bright row to saturated values — closer to macOS
/// Terminal.app's defaults and the One Dark / Tomorrow Night family.
pub const ANSI_16: [(CGFloat, CGFloat, CGFloat); 16] = [
    (0.0784, 0.0980, 0.1176), //  0 black           #14191e
    (0.7726, 0.2354, 0.1568), //  1 red             #c53c28
    (0.1875, 0.7813, 0.3398), //  2 green           #30c757
    (0.8125, 0.6172, 0.1602), //  3 yellow          #cf9e29
    (0.3320, 0.5391, 0.9023), //  4 blue            #5489e6
    (0.7344, 0.3672, 0.8125), //  5 magenta         #bb5ecf
    (0.1602, 0.7188, 0.7461), //  6 cyan            #29b7be
    (0.7810, 0.7811, 0.7810), //  7 white           #c7c7c7
    (0.4078, 0.4078, 0.4078), //  8 bright black    #676767
    (1.0000, 0.3711, 0.3398), //  9 bright red      #ff5f57 (was pink)
    (0.3203, 0.8633, 0.4297), // 10 bright green    #51dc6e
    (1.0000, 0.7656, 0.2148), // 11 bright yellow   #ffc337
    (0.3984, 0.6328, 1.0000), // 12 bright blue     #66a1ff
    (1.0000, 0.4453, 0.7813), // 13 bright magenta  #ff72c8 (was lavender)
    (0.3984, 0.9219, 0.9492), // 14 bright cyan     #66ebf2
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

/// Resolve a cell's attrs to (fg, bg) RGB, honouring SGR reverse + dim.
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
    if attrs.dim {
        // SGR 2 — half-intensity. xterm-style multiply by ~0.55 in
        // sRGB; close enough to "secondary text" weight without
        // tinting hues. Applied AFTER reverse so a reversed-dim cell
        // still reads correctly (rare combo but spec-clean).
        const DIM: CGFloat = 0.55;
        fg = (fg.0 * DIM, fg.1 * DIM, fg.2 * DIM);
    }
    (fg, bg)
}

// CoreText's per-string font fallback resolver — not in the
// `core-text` crate so we declare it directly.
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
    /// Parallel to `fonts`: whether the font carries colour glyphs
    /// (Apple Color Emoji and friends — `kCTFontColorGlyphsTrait`).
    /// Precomputed at intern time so the per-cell render path can route
    /// to the colour atlas without a CoreText call per glyph.
    color: Vec<bool>,
}

fn font_has_color_glyphs(font: &CTFont) -> bool {
    font.symbolic_traits() & kCTFontColorGlyphsTrait != 0
}

impl FontRegistry {
    fn new(base: CTFont) -> Self {
        let name = base.postscript_name();
        let mut by_name = HashMap::new();
        by_name.insert(name, 0);
        let color = vec![font_has_color_glyphs(&base)];
        Self {
            fonts: vec![base],
            by_name,
            color,
        }
    }

    fn intern(&mut self, font: CTFont) -> usize {
        let name = font.postscript_name();
        if let Some(&idx) = self.by_name.get(&name) {
            return idx;
        }
        let idx = self.fonts.len();
        self.by_name.insert(name, idx);
        self.color.push(font_has_color_glyphs(&font));
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
    /// Curated text-font cascade consulted BEFORE letting CoreText's
    /// automatic discovery (`CTFontCreateForString`) pick a fallback
    /// for codepoints the base font lacks.  Without this, CT happily
    /// routes some non-emoji glyphs (notably the Enclosed
    /// Alphanumerics ①②③ and some box-drawing auxiliary chars) to
    /// Apple Color Emoji, which then rasterises them at em-box width
    /// and clips into the 1-cell slot `cluster_width` correctly
    /// assigns them.  Only consulted when the codepoint is NOT
    /// Emoji_Presentation=Yes — actual emoji legitimately want the
    /// colour cascade.  Indices into `self.fonts.fonts`.
    text_fallback_idxs: Vec<usize>,
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

        // Curated text-font cascade.  We deliberately enumerate
        // mono-leaning text fonts first so a 1-cell glyph is more
        // likely to land than a 2-cell-wide emoji bitmap.  Fonts
        // that aren't installed on this system just fail to load
        // and are skipped.
        const TEXT_FALLBACK_NAMES: &[&str] = &[
            "Menlo",
            "SFMono-Regular",
            "Monaco",
            "AppleSDGothicNeo-Regular",
            "PingFangSC-Regular",
            "HiraginoSans-W3",
            "HelveticaNeue",
            "Helvetica",
        ];
        let mut text_fallback_idxs: Vec<usize> = Vec::new();
        for name in TEXT_FALLBACK_NAMES {
            if let Ok(f) = new_from_name(name, FONT_POINT) {
                let idx = fonts.intern(f);
                text_fallback_idxs.push(idx);
            }
        }

        Ok(Self {
            fonts,
            char_cache: HashMap::new(),
            rebuild_count: 0,
            style_font_idx: [0, bold_idx, italic_idx, bold_italic_idx],
            cell_w,
            cell_h,
            ascent,
            text_fallback_idxs,
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
        } else if !marspot_term::emoji_presentation::has_emoji_presentation(ch as u32) {
            // Text-presentation codepoint that the base font lacks.
            // Walk the curated text-font cascade BEFORE letting CT
            // pick — without this, CT routes glyphs like ① ② ③ to
            // Apple Color Emoji, which rasterises them at em-box
            // width and clips into the 1-cell slot.  Iterate over a
            // clone of the indices so we can mutate self.char_cache
            // mid-loop without aliasing.
            let cascade = self.text_fallback_idxs.clone();
            let mut found: Option<(usize, CGGlyph)> = None;
            for idx in cascade {
                if idx == style_idx {
                    continue;
                }
                let f = &self.fonts.fonts[idx];
                let g = lookup_glyph(f, ch);
                if g != 0 {
                    found = Some((idx, g));
                    break;
                }
            }
            if let Some(hit) = found {
                hit
            } else {
                let fallback = create_fallback_font(&base, ch);
                let fb_glyph = lookup_glyph(&fallback, ch);
                let idx = self.fonts.intern(fallback);
                (idx, fb_glyph)
            }
        } else {
            // Emoji_Presentation=Yes — legitimate emoji, use the
            // default cascade so we land on Apple Color Emoji.
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

    /// Whether font `idx` carries colour glyphs (Apple Color Emoji).
    /// Precomputed at intern time — cheap enough for the per-cell path.
    pub fn is_color_font(&self, idx: usize) -> bool {
        self.fonts.color.get(idx).copied().unwrap_or(false)
    }

    pub fn cell_dims(&self) -> (f64, f64) {
        (self.cell_w, self.cell_h)
    }

    /// Approximate resident bytes for instrumentation
    /// (MARSPOT_PROFILE_RSS).  Counts the `(codepoint, style) → glyph`
    /// HashMap and the FontRegistry's bookkeeping (Vec<CTFont>
    /// pointer slots + by_name keys).  CTFont's underlying font data
    /// lives in CoreText's heap and isn't counted here — it shows up
    /// in the `other` bucket.  HashMap bucket overhead is approximated
    /// as one extra `usize` per slot; same simplification as
    /// `GlyphAtlas::approx_bytes`.
    pub fn approx_bytes(&self) -> usize {
        let char_entry =
            std::mem::size_of::<(u32, u8)>() + std::mem::size_of::<(usize, CGGlyph)>();
        let char_cache_bytes =
            self.char_cache.capacity() * (char_entry + std::mem::size_of::<usize>());
        let fonts_vec_bytes =
            self.fonts.fonts.capacity() * std::mem::size_of::<CTFont>();
        let by_name_entry =
            std::mem::size_of::<String>() + std::mem::size_of::<usize>();
        let by_name_bytes = self.fonts.by_name.capacity()
            * (by_name_entry + std::mem::size_of::<usize>());
        let by_name_keys_bytes: usize =
            self.fonts.by_name.keys().map(|k| k.capacity()).sum();
        char_cache_bytes + fonts_vec_bytes + by_name_bytes + by_name_keys_bytes
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

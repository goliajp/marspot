//! UI color — CSS-aligned 8-bit RGB + linear alpha.
//!
//! ## Why this exists
//!
//! Marspot's renderer used to take colours as raw `[f32; 4]` —
//! linear-space RGBA in [0.0, 1.0].  Two problems:
//!
//! 1. No semantic constructor.  Every call site wrote the four
//!    floats by hand.  Hex codes from a design doc needed
//!    manual normalisation (`0xff / 255.0`).  Typos silent.
//! 2. Alpha semantics ambiguous.  Some callers passed 0.5
//!    expecting a blend; others passed 1.0 over a no-blend
//!    pipeline (cells) and got opaque.  No type-level signal
//!    distinguished "I want this transparent" from "I think
//!    alpha is meaningless here."
//!
//! `Color` is 8-bit per channel for R/G/B (the CSS / web /
//! design-tool convention) + `f64` alpha in [0.0, 1.0] (smooth
//! blending math without sRGB rounding).  Construction is via
//! one of three explicit shapes:
//!
//! ```ignore
//! Color::rgb(255, 255, 255)                    // opaque white
//! Color::rgba(255, 255, 255, 0.18)             // 18% transparent white
//! Color::hex("#ffffff")                        // = rgb(255,255,255)
//! Color::hex("#ffffff2e")                      // = rgba(255,255,255, 0x2e/255)
//! Color::hex("#fff")                           // CSS 3-digit shorthand
//! ```
//!
//! The renderer interprets the `[f32; 4]` returned by
//! [`Color::to_rgba_f32`] as sRGB / premultiplied per the
//! Metal pipeline's standard convention.

/// CSS-style RGBA colour. 8-bit R/G/B + `f64` alpha in `[0.0,
/// 1.0]`.
///
/// Construct via `rgb` / `rgba` / `hex`.  Modify via
/// `with_alpha`.  Convert to shader-friendly `[f32; 4]` via
/// `to_rgba_f32`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f64,
}

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0.0 };
    pub const BLACK:       Self = Self { r: 0, g: 0, b: 0, a: 1.0 };
    pub const WHITE:       Self = Self { r: 255, g: 255, b: 255, a: 1.0 };

    /// Opaque colour from RGB triple.
    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// RGB triple + explicit alpha.  Alpha is clamped at use
    /// time, not here, so callers can pass intermediate values
    /// that get further composited.
    #[inline]
    pub const fn rgba(r: u8, g: u8, b: u8, a: f64) -> Self {
        Self { r, g, b, a }
    }

    /// Parse a CSS-style hex colour.
    ///
    /// Accepts:
    /// - `#rgb`        — 3-digit shorthand, each nibble doubled
    ///                   (so `#f0a` ≡ `#ff00aa`)
    /// - `#rgba`       — 4-digit shorthand
    /// - `#rrggbb`     — full 6-digit, alpha = 1.0
    /// - `#rrggbbaa`   — full 8-digit, alpha = `aa / 255`
    ///
    /// `#` is optional.  Case-insensitive.  Returns
    /// [`Color::TRANSPARENT`] on any parse failure — strict
    /// error reporting would force fallible signatures on
    /// every component definition and dirty the call sites.
    pub fn hex(s: &str) -> Self {
        Self::try_hex(s).unwrap_or(Self::TRANSPARENT)
    }

    /// Strict hex parser.  Returns `None` on malformed input —
    /// useful when the caller wants to validate a theme file at
    /// load time.
    pub fn try_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        match s.len() {
            3 => {
                // #rgb → #rrggbb
                let r = parse_nibble(s.as_bytes()[0])?;
                let g = parse_nibble(s.as_bytes()[1])?;
                let b = parse_nibble(s.as_bytes()[2])?;
                Some(Self::rgb(r << 4 | r, g << 4 | g, b << 4 | b))
            }
            4 => {
                // #rgba → #rrggbbaa
                let r = parse_nibble(s.as_bytes()[0])?;
                let g = parse_nibble(s.as_bytes()[1])?;
                let b = parse_nibble(s.as_bytes()[2])?;
                let a = parse_nibble(s.as_bytes()[3])?;
                Some(Self::rgba(
                    r << 4 | r, g << 4 | g, b << 4 | b,
                    ((a << 4 | a) as f64) / 255.0,
                ))
            }
            6 => {
                let r = parse_byte(&s.as_bytes()[0..2])?;
                let g = parse_byte(&s.as_bytes()[2..4])?;
                let b = parse_byte(&s.as_bytes()[4..6])?;
                Some(Self::rgb(r, g, b))
            }
            8 => {
                let r = parse_byte(&s.as_bytes()[0..2])?;
                let g = parse_byte(&s.as_bytes()[2..4])?;
                let b = parse_byte(&s.as_bytes()[4..6])?;
                let a = parse_byte(&s.as_bytes()[6..8])?;
                Some(Self::rgba(r, g, b, (a as f64) / 255.0))
            }
            _ => None,
        }
    }

    /// Return a copy with `a` replaced.  RGB preserved exactly.
    #[inline]
    pub fn with_alpha(self, a: f64) -> Self {
        Self { a, ..self }
    }

    /// Render to the shader's expected `[r, g, b, a]` float
    /// quad.  R/G/B normalised by 255; alpha passed through.
    /// Alpha is clamped to [0, 1] at this boundary so a
    /// caller's intermediate `1.5` doesn't reach the GPU.
    #[inline]
    pub fn to_rgba_f32(self) -> [f32; 4] {
        [
            (self.r as f32) / 255.0,
            (self.g as f32) / 255.0,
            (self.b as f32) / 255.0,
            self.a.clamp(0.0, 1.0) as f32,
        ]
    }
}

#[inline]
fn parse_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn parse_byte(s: &[u8]) -> Option<u8> {
    Some((parse_nibble(s[0])? << 4) | parse_nibble(s[1])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Constructors ───────────────────────────────────────

    #[test]
    fn rgb_is_opaque() {
        assert_eq!(Color::rgb(10, 20, 30), Color { r: 10, g: 20, b: 30, a: 1.0 });
    }

    #[test]
    fn rgba_carries_alpha() {
        assert_eq!(
            Color::rgba(10, 20, 30, 0.5),
            Color { r: 10, g: 20, b: 30, a: 0.5 }
        );
    }

    #[test]
    fn named_constants() {
        assert_eq!(Color::TRANSPARENT.a, 0.0);
        assert_eq!(Color::BLACK, Color::rgb(0, 0, 0));
        assert_eq!(Color::WHITE, Color::rgb(255, 255, 255));
    }

    // ─── hex parser ─────────────────────────────────────────

    #[test]
    fn hex_3_digit_shorthand() {
        assert_eq!(Color::hex("#f0a"), Color::rgb(0xff, 0x00, 0xaa));
        // # is optional
        assert_eq!(Color::hex("f0a"), Color::rgb(0xff, 0x00, 0xaa));
    }

    #[test]
    fn hex_4_digit_shorthand_with_alpha() {
        // # rgba   r=f=255  g=0=0  b=a=170  a=8 / nibble=8 → byte=0x88 → 136/255
        let c = Color::hex("#f0a8");
        assert_eq!((c.r, c.g, c.b), (0xff, 0x00, 0xaa));
        // 0x88 / 0xff = 136 / 255 ≈ 0.533...
        assert!((c.a - 136.0 / 255.0).abs() < 1e-9);
    }

    #[test]
    fn hex_6_digit_full() {
        assert_eq!(Color::hex("#0a141a"), Color::rgb(10, 20, 26));
    }

    #[test]
    fn hex_8_digit_with_alpha() {
        let c = Color::hex("#ffffff2e");
        assert_eq!((c.r, c.g, c.b), (255, 255, 255));
        assert!((c.a - 0x2e as f64 / 255.0).abs() < 1e-9);
    }

    #[test]
    fn hex_case_insensitive() {
        assert_eq!(Color::hex("#ABCDEF"), Color::hex("#abcdef"));
        assert_eq!(Color::hex("#aBcDeF"), Color::hex("#abcdef"));
    }

    #[test]
    fn hex_garbage_returns_transparent_via_hex() {
        // Strict parser used the fallback path.
        assert_eq!(Color::hex("nonsense"), Color::TRANSPARENT);
        assert_eq!(Color::hex(""), Color::TRANSPARENT);
        assert_eq!(Color::hex("#xyz"), Color::TRANSPARENT);
    }

    #[test]
    fn try_hex_garbage_returns_none() {
        assert_eq!(Color::try_hex("nonsense"), None);
        assert_eq!(Color::try_hex("#xyz"), None);
        // 4-digit IS valid, must parse cleanly.
        let c = Color::try_hex("#ffff").expect("valid 4-digit");
        assert_eq!((c.r, c.g, c.b), (255, 255, 255));
        assert!((c.a - 1.0).abs() < 1e-9);
    }

    // ─── with_alpha ─────────────────────────────────────────

    #[test]
    fn with_alpha_preserves_rgb() {
        let c = Color::rgb(10, 20, 30).with_alpha(0.5);
        assert_eq!((c.r, c.g, c.b), (10, 20, 30));
        assert_eq!(c.a, 0.5);
    }

    // ─── to_rgba_f32 ────────────────────────────────────────

    #[test]
    fn to_rgba_f32_normalises_rgb_by_255() {
        let c = Color::rgb(255, 0, 128);
        let v = c.to_rgba_f32();
        assert_eq!(v[0], 1.0);
        assert_eq!(v[1], 0.0);
        assert!((v[2] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(v[3], 1.0);
    }

    #[test]
    fn to_rgba_f32_clamps_alpha_at_boundary() {
        assert_eq!(Color::rgba(0, 0, 0, -0.5).to_rgba_f32()[3], 0.0);
        assert_eq!(Color::rgba(0, 0, 0, 1.5).to_rgba_f32()[3], 1.0);
    }
}

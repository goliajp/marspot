//! Semantic tokens — color / space / radius / typography.
//!
//! Single source of truth.  Components reference tokens by name
//! (`color::accent`, `space::MD`); literal `Color::rgba(...)` in
//! component code is an anti-pattern.
//!
//! Dark theme only for v1; v2 adds Light + HighContrast via a
//! `ThemeId` global per `docs/ui-system-model.md` §12.

use crate::ui::core::{Color, Length};

pub mod color {
    use super::Color;

    // ─── Foreground ───────────────────────────────────────────
    pub const FG:           Color = Color::rgba(232, 238, 248, 1.0);
    pub const FG_MUTED:     Color = Color::rgba(140, 153, 168, 1.0);
    pub const FG_DISABLED:  Color = Color::rgba(115, 122, 133, 1.0);

    // ─── Background ───────────────────────────────────────────
    pub const BG:           Color = Color::rgba( 20,  22,  28, 1.0);
    pub const BG_RAISED:    Color = Color::rgba( 28,  31,  39, 1.0);
    pub const BG_PANEL:     Color = Color::rgba( 33,  36,  43, 1.0);
    pub const BG_SELECTED:  Color = Color::rgba( 51, 107, 173, 1.0);
    pub const BG_HOVER:     Color = Color::rgba( 41,  45,  55, 1.0);

    // ─── Lines ────────────────────────────────────────────────
    pub const BORDER:       Color = Color::rgba( 56,  60,  70, 1.0);
    pub const DIVIDER:      Color = Color::rgba(255, 255, 255, 0.08);
    pub const HAIRLINE:     Color = Color::rgba(255, 255, 255, 0.18);

    // ─── Accent / status ──────────────────────────────────────
    pub const ACCENT:       Color = Color::rgba( 91, 162, 250, 1.0);
    pub const ACCENT_DIM:   Color = Color::rgba(170, 190, 230, 1.0);
    pub const SUCCESS:      Color = Color::rgba( 80, 190, 110, 1.0);
    pub const WARN:         Color = Color::rgba(230, 200,  80, 1.0);
    pub const DANGER:       Color = Color::rgba(220,  60,  60, 1.0);

    // ─── Effects ──────────────────────────────────────────────
    pub const SHADOW:       Color = Color::rgba(  0,   0,   0, 0.55);

    // ─── Helper hints (used inside dev panel etc) ─────────────
    pub const HINT:         Color = Color::rgba(130, 140, 156, 1.0);

    // ─── Light theme palette (P3v-2) ──────────────────────────
    // Used by `themed::color::*` accessors when `ThemeId::Light` is
    // active.  Raw constants below stay Dark for backward compat
    // until callers migrate to themed::*.
    pub mod light {
        use super::Color;
        pub const FG:           Color = Color::rgba( 30,  35,  45, 1.0);
        pub const FG_MUTED:     Color = Color::rgba( 95, 105, 120, 1.0);
        pub const FG_DISABLED:  Color = Color::rgba(170, 178, 188, 1.0);
        pub const BG:           Color = Color::rgba(248, 249, 251, 1.0);
        pub const BG_RAISED:    Color = Color::rgba(255, 255, 255, 1.0);
        pub const BG_PANEL:     Color = Color::rgba(242, 244, 247, 1.0);
        pub const BG_SELECTED:  Color = Color::rgba( 51, 107, 173, 1.0);
        pub const BG_HOVER:     Color = Color::rgba(232, 235, 240, 1.0);
        pub const BORDER:       Color = Color::rgba(205, 210, 218, 1.0);
        pub const DIVIDER:      Color = Color::rgba(  0,   0,   0, 0.06);
        pub const HAIRLINE:     Color = Color::rgba(  0,   0,   0, 0.15);
        pub const ACCENT:       Color = Color::rgba( 51, 107, 220, 1.0);
        pub const ACCENT_DIM:   Color = Color::rgba(120, 145, 210, 1.0);
        pub const SUCCESS:      Color = Color::rgba( 50, 160,  80, 1.0);
        pub const WARN:         Color = Color::rgba(200, 150,  20, 1.0);
        pub const DANGER:       Color = Color::rgba(200,  50,  50, 1.0);
        pub const SHADOW:       Color = Color::rgba(  0,   0,   0, 0.18);
        pub const HINT:         Color = Color::rgba(110, 120, 138, 1.0);
    }
}

/// Themed-aware color lookup — checks `super::current()` then
/// selects from the matching palette.  Components reach for
/// `themed::color::fg()` when they want a theme-switchable color;
/// `color::FG` (const) stays Dark forever for backward compat.
pub mod themed {
    use super::{Color, color};

    pub mod color_fns {
        use super::*;
        pub fn fg() -> Color {
            match super::super::super::current() {
                super::super::super::ThemeId::Light => color::light::FG,
                _ => color::FG,
            }
        }
        pub fn bg() -> Color {
            match super::super::super::current() {
                super::super::super::ThemeId::Light => color::light::BG,
                _ => color::BG,
            }
        }
        pub fn accent() -> Color {
            match super::super::super::current() {
                super::super::super::ThemeId::Light => color::light::ACCENT,
                _ => color::ACCENT,
            }
        }
        pub fn bg_panel() -> Color {
            match super::super::super::current() {
                super::super::super::ThemeId::Light => color::light::BG_PANEL,
                _ => color::BG_PANEL,
            }
        }
        pub fn border() -> Color {
            match super::super::super::current() {
                super::super::super::ThemeId::Light => color::light::BORDER,
                _ => color::BORDER,
            }
        }
    }
}

pub mod space {
    use super::Length;
    pub const XS:  Length = Length::Pt( 4.0);
    pub const SM:  Length = Length::Pt( 8.0);
    pub const MD:  Length = Length::Pt(12.0);
    pub const LG:  Length = Length::Pt(16.0);
    pub const XL:  Length = Length::Pt(24.0);
    pub const XXL: Length = Length::Pt(32.0);
}

pub mod radius {
    use super::Length;
    pub const NONE: Length = Length::Pt(0.0);
    pub const SM:   Length = Length::Pt(3.0);
    pub const MD:   Length = Length::Pt(6.0);
    pub const LG:   Length = Length::Pt(10.0);
    /// Effectively a pill / fully rounded — > any practical box size.
    pub const PILL: Length = Length::Pt(9999.0);
}

/// Semantic text styles — size + weight + color preset.  Components
/// reference `text::CAPTION` etc. instead of composing `.size()` +
/// `.weight()` + `.color()` ad-hoc.  See `docs/ui-system-model.md`
/// §5.4 / §11.
pub mod text {
    use crate::ui::view::{TextSize, TextWeight, TextStyle};
    use super::color;

    pub const CAPTION: TextStyle = TextStyle {
        size:   TextSize::Caption,
        weight: TextWeight::Regular,
        color:  color::FG_MUTED,
    };
    pub const BODY: TextStyle = TextStyle {
        size:   TextSize::Body,
        weight: TextWeight::Regular,
        color:  color::FG,
    };
    pub const HEADER: TextStyle = TextStyle {
        size:   TextSize::Header,
        weight: TextWeight::Bold,
        color:  color::ACCENT_DIM,
    };
    pub const LARGE_HEADER: TextStyle = TextStyle {
        size:   TextSize::LargeHeader,
        weight: TextWeight::Bold,
        color:  color::FG,
    };
    /// Etched / hint-like text — same shape as `CAPTION` but with
    /// the hint color (slightly different muted gray).
    pub const HINT: TextStyle = TextStyle {
        size:   TextSize::Caption,
        weight: TextWeight::Regular,
        color:  color::HINT,
    };
    /// Mono-emphasis (still mono font, but colored like accent).
    pub const CODE: TextStyle = TextStyle {
        size:   TextSize::Body,
        weight: TextWeight::Regular,
        color:  color::ACCENT_DIM,
    };
}

/// Semantic elevation shadows — Material-style E0..E3 tiers.
/// Components reach for `elev::E1` instead of constructing `Shadow`
/// ad-hoc.
pub mod elev {
    use crate::ui::view::Shadow;
    use crate::ui::core::Length;
    use super::color;

    /// No shadow.
    pub const E0: Shadow = Shadow {
        blur: Length::Pt(0.0),
        offset: (Length::Pt(0.0), Length::Pt(0.0)),
        color: color::SHADOW,
    };
    /// Subtle — hint of depth.  Toolbar buttons, low cards.
    pub const E1: Shadow = Shadow {
        blur: Length::Pt(3.0),
        offset: (Length::Pt(0.0), Length::Pt(1.0)),
        color: color::SHADOW,
    };
    /// Standard — panel-on-bg / popover.
    pub const E2: Shadow = Shadow {
        blur: Length::Pt(6.0),
        offset: (Length::Pt(0.0), Length::Pt(2.0)),
        color: color::SHADOW,
    };
    /// High — modal / overlay / floating menu.
    pub const E3: Shadow = Shadow {
        blur: Length::Pt(12.0),
        offset: (Length::Pt(0.0), Length::Pt(4.0)),
        color: color::SHADOW,
    };
}

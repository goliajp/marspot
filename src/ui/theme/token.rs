//! Semantic tokens v4 — color / space / radius / typography /
//! elevation.
//!
//! Single source of truth.  Components reference tokens by name
//! (`color::ACCENT`, `space::MD`); literal `Color::rgba(...)` in
//! component code is an anti-pattern.
//!
//! The constants are the Dark palette.  `light::*` and `hc::*`
//! (high-contrast) hold only the few colours the dev panel shows as
//! swatches; nothing picks between palettes at runtime.

use crate::ui::core::{Color, Length};

// ═════════════════════════════════════════════════════════════
// UI Chrome — color::*  (Dark + light::* + hc::*)
// ═════════════════════════════════════════════════════════════

pub mod color {
    use super::Color;

    // ─── Foreground ───────────────────────────────────────────
    pub const FG:           Color = Color::rgba(232, 238, 248, 1.0);
    pub const FG_MUTED:     Color = Color::rgba(140, 153, 168, 1.0);
    pub const FG_DISABLED:  Color = Color::rgba(115, 122, 133, 1.0);
    pub const FG_INVERSE:   Color = Color::rgba( 20,  22,  28, 1.0);  // on BG_SELECTED
    pub const FG_LINK:      Color = Color::rgba(106, 175, 255, 1.0);

    // ─── Background / Surface levels ──────────────────────────
    pub const BG:           Color = Color::rgba( 20,  22,  28, 1.0);
    pub const BG_RAISED:    Color = Color::rgba( 28,  31,  39, 1.0);
    pub const BG_PANEL:     Color = Color::rgba( 33,  36,  43, 1.0);
    pub const BG_SELECTED:  Color = Color::rgba( 51, 107, 173, 1.0);
    pub const BG_HOVER:     Color = Color::rgba( 41,  45,  55, 1.0);
    pub const SURFACE_3:    Color = Color::rgba( 44,  47,  56, 1.0);  // popover
    pub const SURFACE_4:    Color = Color::rgba( 54,  57,  68, 1.0);  // tooltip

    // ─── Lines ────────────────────────────────────────────────
    pub const BORDER:       Color = Color::rgba( 56,  60,  70, 1.0);
    pub const DIVIDER:      Color = Color::rgba(255, 255, 255, 0.08);
    pub const HAIRLINE:     Color = Color::rgba(255, 255, 255, 0.18);

    // ─── Accent / status ─────────────────────────────────────
    pub const ACCENT:       Color = Color::rgba( 91, 162, 250, 1.0);
    pub const ACCENT_DIM:   Color = Color::rgba(170, 190, 230, 1.0);
    pub const SUCCESS:      Color = Color::rgba( 80, 190, 110, 1.0);
    pub const WARN:         Color = Color::rgba(230, 200,  80, 1.0);
    pub const DANGER:       Color = Color::rgba(220,  60,  60, 1.0);

    // ─── Effects ──────────────────────────────────────────────
    pub const SHADOW:       Color = Color::rgba(  0,   0,   0, 0.55);

    // ─── Helper hints ────────────────────────────────────────
    pub const HINT:         Color = Color::rgba(130, 140, 156, 1.0);

    // ─── High-contrast palette ───────────────────────────────
    pub mod hc {
        use super::Color;
        pub const FG:           Color = Color::rgba(255, 255, 255, 1.0);
        pub const BG:           Color = Color::rgba(  0,   0,   0, 1.0);
        pub const ACCENT:       Color = Color::rgba(255, 220,   0, 1.0);
    }

    // ─── Light theme palette ─────────────────────────────────
    pub mod light {
        use super::Color;
        pub const FG:           Color = Color::rgba( 30,  35,  45, 1.0);
        pub const BG:           Color = Color::rgba(248, 249, 251, 1.0);
        pub const ACCENT:       Color = Color::rgba( 51, 107, 220, 1.0);
    }
}

// ═════════════════════════════════════════════════════════════
// Space / Radius / Text / Elev
// ═════════════════════════════════════════════════════════════

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
    pub const PILL: Length = Length::Pt(9999.0);
}

/// Semantic text styles — size + weight + color preset.
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
    pub const HINT: TextStyle = TextStyle {
        size:   TextSize::Caption,
        weight: TextWeight::Regular,
        color:  color::HINT,
    };
    pub const CODE: TextStyle = TextStyle {
        size:   TextSize::Body,
        weight: TextWeight::Regular,
        color:  color::ACCENT_DIM,
    };
    pub const LINK: TextStyle = TextStyle {
        size:   TextSize::Body,
        weight: TextWeight::Regular,
        color:  color::FG_LINK,
    };
    pub const ERROR: TextStyle = TextStyle {
        size:   TextSize::Body,
        weight: TextWeight::Regular,
        color:  color::DANGER,
    };
}

/// Semantic elevation shadows — Material-style E0..E3.
pub mod elev {
    use crate::ui::view::Shadow;
    use crate::ui::core::Length;
    use super::color;

    pub const E0: Shadow = Shadow {
        blur: Length::Pt(0.0),
        offset: (Length::Pt(0.0), Length::Pt(0.0)),
        color: color::SHADOW,
    };
    pub const E1: Shadow = Shadow {
        blur: Length::Pt(3.0),
        offset: (Length::Pt(0.0), Length::Pt(1.0)),
        color: color::SHADOW,
    };
    pub const E2: Shadow = Shadow {
        blur: Length::Pt(6.0),
        offset: (Length::Pt(0.0), Length::Pt(2.0)),
        color: color::SHADOW,
    };
    pub const E3: Shadow = Shadow {
        blur: Length::Pt(12.0),
        offset: (Length::Pt(0.0), Length::Pt(4.0)),
        color: color::SHADOW,
    };
}

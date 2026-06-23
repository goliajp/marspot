//! Semantic tokens v4 — color / space / radius / typography /
//! terminal palette / motion / border / layer / elevation.
//!
//! Single source of truth.  Components reference tokens by name
//! (`color::ACCENT`, `space::MD`, `terminal::ansi::RED`); literal
//! `Color::rgba(...)` in component code is an anti-pattern.
//!
//! Three theme palettes: `Dark` (default constants), `light::*`,
//! `hc::*` (high-contrast).  `themed::*` closures dispatch by
//! `super::current()`.  See `docs/ui-component-library.md` §3.

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
    pub const SURFACE_0:    Color = BG;
    pub const SURFACE_1:    Color = BG_RAISED;
    pub const SURFACE_2:    Color = BG_PANEL;
    pub const SURFACE_3:    Color = Color::rgba( 44,  47,  56, 1.0);  // popover
    pub const SURFACE_4:    Color = Color::rgba( 54,  57,  68, 1.0);  // tooltip
    pub const OVERLAY:      Color = Color::rgba(  0,   0,   0, 0.50); // modal scrim

    // ─── Lines ────────────────────────────────────────────────
    pub const BORDER:       Color = Color::rgba( 56,  60,  70, 1.0);
    pub const DIVIDER:      Color = Color::rgba(255, 255, 255, 0.08);
    pub const HAIRLINE:     Color = Color::rgba(255, 255, 255, 0.18);

    // ─── Accent / status / 4-level severity ──────────────────
    pub const ACCENT:       Color = Color::rgba( 91, 162, 250, 1.0);
    pub const ACCENT_DIM:   Color = Color::rgba(170, 190, 230, 1.0);
    pub const INFO:         Color = ACCENT;
    pub const SUCCESS:      Color = Color::rgba( 80, 190, 110, 1.0);
    pub const WARN:         Color = Color::rgba(230, 200,  80, 1.0);
    pub const DANGER:       Color = Color::rgba(220,  60,  60, 1.0);
    pub const CRITICAL:     Color = Color::rgba(180,  40,  40, 1.0);  // 深红 / 系统级

    // ─── Effects ──────────────────────────────────────────────
    pub const SHADOW:       Color = Color::rgba(  0,   0,   0, 0.55);

    // ─── Helper hints ────────────────────────────────────────
    pub const HINT:         Color = Color::rgba(130, 140, 156, 1.0);

    // ─── Focus / Disabled ────────────────────────────────────
    pub const FOCUS_RING:   Color = Color::rgba( 91, 162, 250, 0.6);
    pub const DISABLED_BG:  Color = Color::rgba( 30,  33,  40, 1.0);
    pub const DISABLED_FG:  Color = Color::rgba( 90, 100, 115, 1.0);

    // ─── Tab strip ────────────────────────────────────────────
    pub const TAB_ACTIVE_BG:    Color = BG_RAISED;
    pub const TAB_ACTIVE_FG:    Color = FG;
    pub const TAB_INACTIVE_BG:  Color = BG;
    pub const TAB_INACTIVE_FG:  Color = FG_MUTED;
    pub const TAB_HOVER_BG:     Color = BG_HOVER;

    // ─── Sidebar ──────────────────────────────────────────────
    pub const SIDEBAR_BG:        Color = Color::rgba( 25,  28,  34, 1.0);
    pub const SIDEBAR_FG:        Color = FG_MUTED;
    pub const SIDEBAR_ACTIVE_BG: Color = BG_SELECTED;
    pub const SIDEBAR_ACTIVE_FG: Color = FG;

    // ─── Status bar ──────────────────────────────────────────
    pub const STATUS_BAR_BG:    Color = Color::rgba( 22,  24,  30, 1.0);
    pub const STATUS_BAR_FG:    Color = FG_MUTED;

    // ─── Diff colours ────────────────────────────────────────
    pub const DIFF_ADD_BG:      Color = Color::rgba( 30,  90,  60, 0.40);
    pub const DIFF_ADD_FG:      Color = Color::rgba(100, 220, 140, 1.0);
    pub const DIFF_REMOVE_BG:   Color = Color::rgba(110,  40,  40, 0.40);
    pub const DIFF_REMOVE_FG:   Color = Color::rgba(240, 130, 130, 1.0);
    pub const DIFF_CHANGE_BG:   Color = Color::rgba( 90,  80,  30, 0.40);

    // ─── High-contrast palette ───────────────────────────────
    pub mod hc {
        use super::Color;
        pub const FG:           Color = Color::rgba(255, 255, 255, 1.0);
        pub const FG_MUTED:     Color = Color::rgba(200, 200, 200, 1.0);
        pub const FG_DISABLED:  Color = Color::rgba(140, 140, 140, 1.0);
        pub const FG_INVERSE:   Color = Color::rgba(  0,   0,   0, 1.0);
        pub const FG_LINK:      Color = Color::rgba(255, 220,   0, 1.0);
        pub const BG:           Color = Color::rgba(  0,   0,   0, 1.0);
        pub const BG_RAISED:    Color = Color::rgba( 25,  25,  25, 1.0);
        pub const BG_PANEL:     Color = Color::rgba( 15,  15,  15, 1.0);
        pub const BG_SELECTED:  Color = Color::rgba(255, 255, 255, 1.0);
        pub const BG_HOVER:     Color = Color::rgba( 50,  50,  50, 1.0);
        pub const SURFACE_0:    Color = BG;
        pub const SURFACE_1:    Color = BG_RAISED;
        pub const SURFACE_2:    Color = BG_PANEL;
        pub const SURFACE_3:    Color = Color::rgba( 35,  35,  35, 1.0);
        pub const SURFACE_4:    Color = Color::rgba( 45,  45,  45, 1.0);
        pub const OVERLAY:      Color = Color::rgba(  0,   0,   0, 0.75);
        pub const BORDER:       Color = Color::rgba(255, 255, 255, 1.0);
        pub const DIVIDER:      Color = Color::rgba(255, 255, 255, 0.40);
        pub const HAIRLINE:     Color = Color::rgba(255, 255, 255, 0.70);
        pub const ACCENT:       Color = Color::rgba(255, 220,   0, 1.0);
        pub const ACCENT_DIM:   Color = Color::rgba(220, 200,   0, 1.0);
        pub const INFO:         Color = ACCENT;
        pub const SUCCESS:      Color = Color::rgba(  0, 255,   0, 1.0);
        pub const WARN:         Color = Color::rgba(255, 200,   0, 1.0);
        pub const DANGER:       Color = Color::rgba(255,   0,   0, 1.0);
        pub const CRITICAL:     Color = Color::rgba(200,   0,   0, 1.0);
        pub const SHADOW:       Color = Color::rgba(  0,   0,   0, 0.95);
        pub const HINT:         Color = Color::rgba(200, 200, 200, 1.0);
        pub const FOCUS_RING:   Color = Color::rgba(255, 220,   0, 1.0);
        pub const DISABLED_BG:  Color = Color::rgba( 30,  30,  30, 1.0);
        pub const DISABLED_FG:  Color = Color::rgba(100, 100, 100, 1.0);
        pub const TAB_ACTIVE_BG:    Color = Color::rgba( 50,  50,  50, 1.0);
        pub const TAB_ACTIVE_FG:    Color = FG;
        pub const TAB_INACTIVE_BG:  Color = BG;
        pub const TAB_INACTIVE_FG:  Color = FG_MUTED;
        pub const TAB_HOVER_BG:     Color = Color::rgba( 70,  70,  70, 1.0);
        pub const SIDEBAR_BG:        Color = Color::rgba(  5,   5,   5, 1.0);
        pub const SIDEBAR_FG:        Color = FG;
        pub const SIDEBAR_ACTIVE_BG: Color = ACCENT;
        pub const SIDEBAR_ACTIVE_FG: Color = FG_INVERSE;
        pub const STATUS_BAR_BG:    Color = Color::rgba( 10,  10,  10, 1.0);
        pub const STATUS_BAR_FG:    Color = FG;
        pub const DIFF_ADD_BG:      Color = Color::rgba(  0,  80,   0, 1.0);
        pub const DIFF_ADD_FG:      Color = Color::rgba(  0, 255,   0, 1.0);
        pub const DIFF_REMOVE_BG:   Color = Color::rgba(120,   0,   0, 1.0);
        pub const DIFF_REMOVE_FG:   Color = Color::rgba(255,  60,  60, 1.0);
        pub const DIFF_CHANGE_BG:   Color = Color::rgba(120,  90,   0, 1.0);
    }

    // ─── Light theme palette ─────────────────────────────────
    pub mod light {
        use super::Color;
        pub const FG:           Color = Color::rgba( 30,  35,  45, 1.0);
        pub const FG_MUTED:     Color = Color::rgba( 95, 105, 120, 1.0);
        pub const FG_DISABLED:  Color = Color::rgba(170, 178, 188, 1.0);
        pub const FG_INVERSE:   Color = Color::rgba(255, 255, 255, 1.0);
        pub const FG_LINK:      Color = Color::rgba( 30,  85, 200, 1.0);
        pub const BG:           Color = Color::rgba(248, 249, 251, 1.0);
        pub const BG_RAISED:    Color = Color::rgba(255, 255, 255, 1.0);
        pub const BG_PANEL:     Color = Color::rgba(242, 244, 247, 1.0);
        pub const BG_SELECTED:  Color = Color::rgba( 51, 107, 173, 1.0);
        pub const BG_HOVER:     Color = Color::rgba(232, 235, 240, 1.0);
        pub const SURFACE_0:    Color = BG;
        pub const SURFACE_1:    Color = BG_RAISED;
        pub const SURFACE_2:    Color = BG_PANEL;
        pub const SURFACE_3:    Color = Color::rgba(238, 240, 244, 1.0);
        pub const SURFACE_4:    Color = Color::rgba(230, 233, 238, 1.0);
        pub const OVERLAY:      Color = Color::rgba(  0,   0,   0, 0.35);
        pub const BORDER:       Color = Color::rgba(205, 210, 218, 1.0);
        pub const DIVIDER:      Color = Color::rgba(  0,   0,   0, 0.06);
        pub const HAIRLINE:     Color = Color::rgba(  0,   0,   0, 0.15);
        pub const ACCENT:       Color = Color::rgba( 51, 107, 220, 1.0);
        pub const ACCENT_DIM:   Color = Color::rgba(120, 145, 210, 1.0);
        pub const INFO:         Color = ACCENT;
        pub const SUCCESS:      Color = Color::rgba( 50, 160,  80, 1.0);
        pub const WARN:         Color = Color::rgba(200, 150,  20, 1.0);
        pub const DANGER:       Color = Color::rgba(200,  50,  50, 1.0);
        pub const CRITICAL:     Color = Color::rgba(150,  30,  30, 1.0);
        pub const SHADOW:       Color = Color::rgba(  0,   0,   0, 0.18);
        pub const HINT:         Color = Color::rgba(110, 120, 138, 1.0);
        pub const FOCUS_RING:   Color = Color::rgba( 51, 107, 220, 0.5);
        pub const DISABLED_BG:  Color = Color::rgba(240, 242, 245, 1.0);
        pub const DISABLED_FG:  Color = Color::rgba(170, 178, 188, 1.0);
        pub const TAB_ACTIVE_BG:    Color = BG_RAISED;
        pub const TAB_ACTIVE_FG:    Color = FG;
        pub const TAB_INACTIVE_BG:  Color = BG;
        pub const TAB_INACTIVE_FG:  Color = FG_MUTED;
        pub const TAB_HOVER_BG:     Color = BG_HOVER;
        pub const SIDEBAR_BG:        Color = Color::rgba(238, 241, 245, 1.0);
        pub const SIDEBAR_FG:        Color = FG_MUTED;
        pub const SIDEBAR_ACTIVE_BG: Color = BG_SELECTED;
        pub const SIDEBAR_ACTIVE_FG: Color = FG_INVERSE;
        pub const STATUS_BAR_BG:    Color = Color::rgba(232, 235, 240, 1.0);
        pub const STATUS_BAR_FG:    Color = FG_MUTED;
        pub const DIFF_ADD_BG:      Color = Color::rgba(180, 230, 195, 0.5);
        pub const DIFF_ADD_FG:      Color = Color::rgba( 30, 120,  60, 1.0);
        pub const DIFF_REMOVE_BG:   Color = Color::rgba(250, 200, 200, 0.5);
        pub const DIFF_REMOVE_FG:   Color = Color::rgba(170,  30,  30, 1.0);
        pub const DIFF_CHANGE_BG:   Color = Color::rgba(245, 230, 175, 0.5);
    }
}

// ═════════════════════════════════════════════════════════════
// Terminal palette — terminal::*  (Dark + light::* + hc::*)
// ═════════════════════════════════════════════════════════════

pub mod terminal {
    use super::Color;

    // ─── Core ─────────────────────────────────────────────────
    pub const BG:           Color = Color::rgba( 20,  22,  28, 1.0);
    pub const FG:           Color = Color::rgba(232, 238, 248, 1.0);
    pub const CURSOR_BG:    Color = Color::rgba(180, 195, 220, 1.0);
    pub const CURSOR_FG:    Color = Color::rgba( 20,  22,  28, 1.0);
    pub const SELECTION_BG: Color = Color::rgba( 51, 107, 173, 0.6);
    pub const SELECTION_FG: Color = Color::rgba(255, 255, 255, 1.0);
    pub const LINK:         Color = Color::rgba(106, 175, 255, 1.0);
    pub const BOLD_FG:      Color = Color::rgba(255, 255, 255, 1.0);

    // ─── ANSI 16-color (xterm-256 baseline) ──────────────────
    pub mod ansi {
        use super::Color;
        pub const BLACK:   Color = Color::rgba(  0,   0,   0, 1.0);
        pub const RED:     Color = Color::rgba(205,  49,  49, 1.0);
        pub const GREEN:   Color = Color::rgba( 13, 188, 121, 1.0);
        pub const YELLOW:  Color = Color::rgba(229, 229,  16, 1.0);
        pub const BLUE:    Color = Color::rgba( 36, 114, 200, 1.0);
        pub const MAGENTA: Color = Color::rgba(188,  63, 188, 1.0);
        pub const CYAN:    Color = Color::rgba( 17, 168, 205, 1.0);
        pub const WHITE:   Color = Color::rgba(229, 229, 229, 1.0);

        pub mod bright {
            use super::Color;
            pub const BLACK:   Color = Color::rgba(102, 102, 102, 1.0);
            pub const RED:     Color = Color::rgba(241,  76,  76, 1.0);
            pub const GREEN:   Color = Color::rgba( 35, 209, 139, 1.0);
            pub const YELLOW:  Color = Color::rgba(245, 245,  67, 1.0);
            pub const BLUE:    Color = Color::rgba( 59, 142, 234, 1.0);
            pub const MAGENTA: Color = Color::rgba(214, 112, 214, 1.0);
            pub const CYAN:    Color = Color::rgba( 41, 184, 219, 1.0);
            pub const WHITE:   Color = Color::rgba(229, 229, 229, 1.0);
        }
    }

    // ─── Search match colours ────────────────────────────────
    pub mod search {
        use super::Color;
        pub const MATCH:         Color = Color::rgba( 60,  90, 140, 0.7);
        pub const MATCH_CURRENT: Color = Color::rgba(120, 170, 240, 0.85);
    }

    // ─── Light theme terminal palette ────────────────────────
    pub mod light {
        use super::Color;
        pub const BG:           Color = Color::rgba(248, 249, 251, 1.0);
        pub const FG:           Color = Color::rgba( 30,  35,  45, 1.0);
        pub const CURSOR_BG:    Color = Color::rgba( 51, 107, 220, 1.0);
        pub const CURSOR_FG:    Color = Color::rgba(255, 255, 255, 1.0);
        pub const SELECTION_BG: Color = Color::rgba( 51, 107, 173, 0.30);
        pub const SELECTION_FG: Color = FG;
        pub const LINK:         Color = Color::rgba( 30,  85, 200, 1.0);
        pub const BOLD_FG:      Color = Color::rgba(  0,   0,   0, 1.0);

        pub mod ansi {
            use super::Color;
            pub const BLACK:   Color = Color::rgba(  0,   0,   0, 1.0);
            pub const RED:     Color = Color::rgba(170,  20,  20, 1.0);
            pub const GREEN:   Color = Color::rgba( 10, 130,  80, 1.0);
            pub const YELLOW:  Color = Color::rgba(180, 150,  20, 1.0);
            pub const BLUE:    Color = Color::rgba( 30,  80, 180, 1.0);
            pub const MAGENTA: Color = Color::rgba(150,  40, 150, 1.0);
            pub const CYAN:    Color = Color::rgba( 20, 130, 160, 1.0);
            pub const WHITE:   Color = Color::rgba(210, 210, 210, 1.0);

            pub mod bright {
                use super::Color;
                pub const BLACK:   Color = Color::rgba(100, 100, 100, 1.0);
                pub const RED:     Color = Color::rgba(220,  40,  40, 1.0);
                pub const GREEN:   Color = Color::rgba( 30, 180, 110, 1.0);
                pub const YELLOW:  Color = Color::rgba(230, 200,  30, 1.0);
                pub const BLUE:    Color = Color::rgba( 50, 110, 210, 1.0);
                pub const MAGENTA: Color = Color::rgba(190,  70, 190, 1.0);
                pub const CYAN:    Color = Color::rgba( 30, 160, 200, 1.0);
                pub const WHITE:   Color = Color::rgba(240, 240, 240, 1.0);
            }
        }

        pub mod search {
            use super::Color;
            pub const MATCH:         Color = Color::rgba(255, 230, 100, 0.85);
            pub const MATCH_CURRENT: Color = Color::rgba(255, 180,   0, 0.95);
        }
    }

    // ─── High-contrast theme terminal palette ────────────────
    pub mod hc {
        use super::Color;
        pub const BG:           Color = Color::rgba(  0,   0,   0, 1.0);
        pub const FG:           Color = Color::rgba(255, 255, 255, 1.0);
        pub const CURSOR_BG:    Color = Color::rgba(255, 220,   0, 1.0);
        pub const CURSOR_FG:    Color = Color::rgba(  0,   0,   0, 1.0);
        pub const SELECTION_BG: Color = Color::rgba(255, 255, 255, 0.95);
        pub const SELECTION_FG: Color = Color::rgba(  0,   0,   0, 1.0);
        pub const LINK:         Color = Color::rgba(255, 255,   0, 1.0);
        pub const BOLD_FG:      Color = Color::rgba(255, 255, 255, 1.0);

        pub mod ansi {
            use super::Color;
            pub const BLACK:   Color = Color::rgba(  0,   0,   0, 1.0);
            pub const RED:     Color = Color::rgba(255,   0,   0, 1.0);
            pub const GREEN:   Color = Color::rgba(  0, 255,   0, 1.0);
            pub const YELLOW:  Color = Color::rgba(255, 255,   0, 1.0);
            pub const BLUE:    Color = Color::rgba(  0, 100, 255, 1.0);
            pub const MAGENTA: Color = Color::rgba(255,   0, 255, 1.0);
            pub const CYAN:    Color = Color::rgba(  0, 255, 255, 1.0);
            pub const WHITE:   Color = Color::rgba(255, 255, 255, 1.0);

            pub mod bright {
                use super::Color;
                pub const BLACK:   Color = Color::rgba(120, 120, 120, 1.0);
                pub const RED:     Color = Color::rgba(255,  80,  80, 1.0);
                pub const GREEN:   Color = Color::rgba( 80, 255,  80, 1.0);
                pub const YELLOW:  Color = Color::rgba(255, 255,  80, 1.0);
                pub const BLUE:    Color = Color::rgba( 80, 130, 255, 1.0);
                pub const MAGENTA: Color = Color::rgba(255,  80, 255, 1.0);
                pub const CYAN:    Color = Color::rgba( 80, 255, 255, 1.0);
                pub const WHITE:   Color = Color::rgba(255, 255, 255, 1.0);
            }
        }

        pub mod search {
            use super::Color;
            pub const MATCH:         Color = Color::rgba(255, 220,   0, 0.6);
            pub const MATCH_CURRENT: Color = Color::rgba(255, 220,   0, 1.0);
        }
    }
}

// ═════════════════════════════════════════════════════════════
// Space / Radius / Text / Elev / Motion / Border / Layer
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

/// Border-width tokens.  Use with `.border(width, color)` modifier.
pub mod border {
    use super::Length;
    pub const NONE:     Length = Length::Pt(0.0);
    pub const HAIRLINE: Length = Length::Pt(0.5);
    pub const THIN:     Length = Length::Pt(1.0);
    pub const MEDIUM:   Length = Length::Pt(2.0);
    pub const THICK:    Length = Length::Pt(3.0);
}

/// Motion / animation duration tokens.  Pair with `AnimCurve` from
/// `theme::motion::curve`.  Durations in milliseconds.
pub mod motion {
    pub const INSTANT:   f64 = 0.0;
    pub const FAST:      f64 = 120.0;
    pub const NORMAL:    f64 = 200.0;
    pub const SLOW:      f64 = 350.0;
    pub const VERY_SLOW: f64 = 700.0;

    /// Curve preset bag — convenience re-export.
    pub mod curve {
        use crate::ui::view::AnimCurve;
        pub const STANDARD: AnimCurve = AnimCurve::EaseInOut;
        pub const DECEL:    AnimCurve = AnimCurve::EaseOut;
        pub const ACCEL:    AnimCurve = AnimCurve::EaseIn;
        pub const LINEAR:   AnimCurve = AnimCurve::Linear;
        pub const SPRING:   AnimCurve = AnimCurve::Spring { bounce: 1.0 };
    }
}

/// Z-index / paint layer tokens.  Use with `.z_index(layer::POPOVER)`
/// inside a ZStack to compose overlays at consistent depths.
pub mod layer {
    pub const CONTENT: i32 = 0;
    pub const STATUS:  i32 = 10;
    pub const STICKY:  i32 = 100;
    pub const TOOLBAR: i32 = 200;
    pub const POPOVER: i32 = 1000;
    pub const MODAL:   i32 = 2000;
    pub const TOAST:   i32 = 3000;
    pub const TOOLTIP: i32 = 4000;
    pub const SYSTEM:  i32 = 9999;
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
    pub const LARGE_HEADER: TextStyle = TextStyle {
        size:   TextSize::LargeHeader,
        weight: TextWeight::Bold,
        color:  color::FG,
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

// ═════════════════════════════════════════════════════════════
// themed::* — runtime palette dispatch by ThemeId
// ═════════════════════════════════════════════════════════════

/// `themed::*` looks up the active theme via `theme::current()` and
/// returns the matching `Color` from `Dark` / `light::*` / `hc::*`.
/// Components reach for `themed::color::fg()` when they want a
/// theme-switchable color;  the const `color::FG` stays Dark forever
/// for backward compat.
pub mod themed {
    pub mod color {
        use crate::ui::core::Color;
        use crate::ui::theme::{current, ThemeId, color as p};

        #[inline]
        fn pick(dark: Color, light: Color, hc: Color) -> Color {
            match current() {
                ThemeId::Light        => light,
                ThemeId::HighContrast => hc,
                _                     => dark,
            }
        }

        // ─── Foreground ──────────────────────────────────────
        pub fn fg()           -> Color { pick(p::FG,           p::light::FG,           p::hc::FG) }
        pub fn fg_muted()     -> Color { pick(p::FG_MUTED,     p::light::FG_MUTED,     p::hc::FG_MUTED) }
        pub fn fg_disabled()  -> Color { pick(p::FG_DISABLED,  p::light::FG_DISABLED,  p::hc::FG_DISABLED) }
        pub fn fg_inverse()   -> Color { pick(p::FG_INVERSE,   p::light::FG_INVERSE,   p::hc::FG_INVERSE) }
        pub fn fg_link()      -> Color { pick(p::FG_LINK,      p::light::FG_LINK,      p::hc::FG_LINK) }

        // ─── Surfaces ────────────────────────────────────────
        pub fn bg()           -> Color { pick(p::BG,           p::light::BG,           p::hc::BG) }
        pub fn bg_raised()    -> Color { pick(p::BG_RAISED,    p::light::BG_RAISED,    p::hc::BG_RAISED) }
        pub fn bg_panel()     -> Color { pick(p::BG_PANEL,     p::light::BG_PANEL,     p::hc::BG_PANEL) }
        pub fn bg_selected()  -> Color { pick(p::BG_SELECTED,  p::light::BG_SELECTED,  p::hc::BG_SELECTED) }
        pub fn bg_hover()     -> Color { pick(p::BG_HOVER,     p::light::BG_HOVER,     p::hc::BG_HOVER) }
        pub fn surface_0()    -> Color { pick(p::SURFACE_0,    p::light::SURFACE_0,    p::hc::SURFACE_0) }
        pub fn surface_1()    -> Color { pick(p::SURFACE_1,    p::light::SURFACE_1,    p::hc::SURFACE_1) }
        pub fn surface_2()    -> Color { pick(p::SURFACE_2,    p::light::SURFACE_2,    p::hc::SURFACE_2) }
        pub fn surface_3()    -> Color { pick(p::SURFACE_3,    p::light::SURFACE_3,    p::hc::SURFACE_3) }
        pub fn surface_4()    -> Color { pick(p::SURFACE_4,    p::light::SURFACE_4,    p::hc::SURFACE_4) }
        pub fn overlay()      -> Color { pick(p::OVERLAY,      p::light::OVERLAY,      p::hc::OVERLAY) }

        // ─── Lines ───────────────────────────────────────────
        pub fn border()       -> Color { pick(p::BORDER,       p::light::BORDER,       p::hc::BORDER) }
        pub fn divider()      -> Color { pick(p::DIVIDER,      p::light::DIVIDER,      p::hc::DIVIDER) }
        pub fn hairline()     -> Color { pick(p::HAIRLINE,     p::light::HAIRLINE,     p::hc::HAIRLINE) }

        // ─── Accent / severity ───────────────────────────────
        pub fn accent()       -> Color { pick(p::ACCENT,       p::light::ACCENT,       p::hc::ACCENT) }
        pub fn accent_dim()   -> Color { pick(p::ACCENT_DIM,   p::light::ACCENT_DIM,   p::hc::ACCENT_DIM) }
        pub fn info()         -> Color { pick(p::INFO,         p::light::INFO,         p::hc::INFO) }
        pub fn success()      -> Color { pick(p::SUCCESS,      p::light::SUCCESS,      p::hc::SUCCESS) }
        pub fn warn()         -> Color { pick(p::WARN,         p::light::WARN,         p::hc::WARN) }
        pub fn danger()       -> Color { pick(p::DANGER,       p::light::DANGER,       p::hc::DANGER) }
        pub fn critical()     -> Color { pick(p::CRITICAL,     p::light::CRITICAL,     p::hc::CRITICAL) }

        // ─── Focus / Disabled ────────────────────────────────
        pub fn focus_ring()   -> Color { pick(p::FOCUS_RING,   p::light::FOCUS_RING,   p::hc::FOCUS_RING) }
        pub fn disabled_bg()  -> Color { pick(p::DISABLED_BG,  p::light::DISABLED_BG,  p::hc::DISABLED_BG) }
        pub fn disabled_fg()  -> Color { pick(p::DISABLED_FG,  p::light::DISABLED_FG,  p::hc::DISABLED_FG) }

        // ─── Tabs ────────────────────────────────────────────
        pub fn tab_active_bg()    -> Color { pick(p::TAB_ACTIVE_BG,   p::light::TAB_ACTIVE_BG,   p::hc::TAB_ACTIVE_BG) }
        pub fn tab_active_fg()    -> Color { pick(p::TAB_ACTIVE_FG,   p::light::TAB_ACTIVE_FG,   p::hc::TAB_ACTIVE_FG) }
        pub fn tab_inactive_bg()  -> Color { pick(p::TAB_INACTIVE_BG, p::light::TAB_INACTIVE_BG, p::hc::TAB_INACTIVE_BG) }
        pub fn tab_inactive_fg()  -> Color { pick(p::TAB_INACTIVE_FG, p::light::TAB_INACTIVE_FG, p::hc::TAB_INACTIVE_FG) }
        pub fn tab_hover_bg()     -> Color { pick(p::TAB_HOVER_BG,    p::light::TAB_HOVER_BG,    p::hc::TAB_HOVER_BG) }

        // ─── Sidebar ─────────────────────────────────────────
        pub fn sidebar_bg()        -> Color { pick(p::SIDEBAR_BG,        p::light::SIDEBAR_BG,        p::hc::SIDEBAR_BG) }
        pub fn sidebar_fg()        -> Color { pick(p::SIDEBAR_FG,        p::light::SIDEBAR_FG,        p::hc::SIDEBAR_FG) }
        pub fn sidebar_active_bg() -> Color { pick(p::SIDEBAR_ACTIVE_BG, p::light::SIDEBAR_ACTIVE_BG, p::hc::SIDEBAR_ACTIVE_BG) }
        pub fn sidebar_active_fg() -> Color { pick(p::SIDEBAR_ACTIVE_FG, p::light::SIDEBAR_ACTIVE_FG, p::hc::SIDEBAR_ACTIVE_FG) }

        // ─── Status bar ──────────────────────────────────────
        pub fn status_bar_bg()    -> Color { pick(p::STATUS_BAR_BG, p::light::STATUS_BAR_BG, p::hc::STATUS_BAR_BG) }
        pub fn status_bar_fg()    -> Color { pick(p::STATUS_BAR_FG, p::light::STATUS_BAR_FG, p::hc::STATUS_BAR_FG) }

        // ─── Diff ────────────────────────────────────────────
        pub fn diff_add_bg()      -> Color { pick(p::DIFF_ADD_BG,    p::light::DIFF_ADD_BG,    p::hc::DIFF_ADD_BG) }
        pub fn diff_add_fg()      -> Color { pick(p::DIFF_ADD_FG,    p::light::DIFF_ADD_FG,    p::hc::DIFF_ADD_FG) }
        pub fn diff_remove_bg()   -> Color { pick(p::DIFF_REMOVE_BG, p::light::DIFF_REMOVE_BG, p::hc::DIFF_REMOVE_BG) }
        pub fn diff_remove_fg()   -> Color { pick(p::DIFF_REMOVE_FG, p::light::DIFF_REMOVE_FG, p::hc::DIFF_REMOVE_FG) }
        pub fn diff_change_bg()   -> Color { pick(p::DIFF_CHANGE_BG, p::light::DIFF_CHANGE_BG, p::hc::DIFF_CHANGE_BG) }

        // ─── Misc ────────────────────────────────────────────
        pub fn shadow()       -> Color { pick(p::SHADOW,       p::light::SHADOW,       p::hc::SHADOW) }
        pub fn hint()         -> Color { pick(p::HINT,         p::light::HINT,         p::hc::HINT) }
    }

    pub mod terminal {
        use crate::ui::core::Color;
        use crate::ui::theme::{current, ThemeId, terminal as t};

        #[inline]
        fn pick(dark: Color, light: Color, hc: Color) -> Color {
            match current() {
                ThemeId::Light        => light,
                ThemeId::HighContrast => hc,
                _                     => dark,
            }
        }

        pub fn bg()           -> Color { pick(t::BG,           t::light::BG,           t::hc::BG) }
        pub fn fg()           -> Color { pick(t::FG,           t::light::FG,           t::hc::FG) }
        pub fn cursor_bg()    -> Color { pick(t::CURSOR_BG,    t::light::CURSOR_BG,    t::hc::CURSOR_BG) }
        pub fn cursor_fg()    -> Color { pick(t::CURSOR_FG,    t::light::CURSOR_FG,    t::hc::CURSOR_FG) }
        pub fn selection_bg() -> Color { pick(t::SELECTION_BG, t::light::SELECTION_BG, t::hc::SELECTION_BG) }
        pub fn selection_fg() -> Color { pick(t::SELECTION_FG, t::light::SELECTION_FG, t::hc::SELECTION_FG) }
        pub fn link()         -> Color { pick(t::LINK,         t::light::LINK,         t::hc::LINK) }
        pub fn bold_fg()      -> Color { pick(t::BOLD_FG,      t::light::BOLD_FG,      t::hc::BOLD_FG) }

        pub mod ansi {
            use crate::ui::core::Color;
            use crate::ui::theme::{current, ThemeId, terminal as t};

            #[inline]
            fn pick(dark: Color, light: Color, hc: Color) -> Color {
                match current() {
                    ThemeId::Light        => light,
                    ThemeId::HighContrast => hc,
                    _                     => dark,
                }
            }

            pub fn black()   -> Color { pick(t::ansi::BLACK,   t::light::ansi::BLACK,   t::hc::ansi::BLACK) }
            pub fn red()     -> Color { pick(t::ansi::RED,     t::light::ansi::RED,     t::hc::ansi::RED) }
            pub fn green()   -> Color { pick(t::ansi::GREEN,   t::light::ansi::GREEN,   t::hc::ansi::GREEN) }
            pub fn yellow()  -> Color { pick(t::ansi::YELLOW,  t::light::ansi::YELLOW,  t::hc::ansi::YELLOW) }
            pub fn blue()    -> Color { pick(t::ansi::BLUE,    t::light::ansi::BLUE,    t::hc::ansi::BLUE) }
            pub fn magenta() -> Color { pick(t::ansi::MAGENTA, t::light::ansi::MAGENTA, t::hc::ansi::MAGENTA) }
            pub fn cyan()    -> Color { pick(t::ansi::CYAN,    t::light::ansi::CYAN,    t::hc::ansi::CYAN) }
            pub fn white()   -> Color { pick(t::ansi::WHITE,   t::light::ansi::WHITE,   t::hc::ansi::WHITE) }

            pub mod bright {
                use crate::ui::core::Color;
                use crate::ui::theme::{current, ThemeId, terminal as t};

                #[inline]
                fn pick(dark: Color, light: Color, hc: Color) -> Color {
                    match current() {
                        ThemeId::Light        => light,
                        ThemeId::HighContrast => hc,
                        _                     => dark,
                    }
                }
                pub fn black()   -> Color { pick(t::ansi::bright::BLACK,   t::light::ansi::bright::BLACK,   t::hc::ansi::bright::BLACK) }
                pub fn red()     -> Color { pick(t::ansi::bright::RED,     t::light::ansi::bright::RED,     t::hc::ansi::bright::RED) }
                pub fn green()   -> Color { pick(t::ansi::bright::GREEN,   t::light::ansi::bright::GREEN,   t::hc::ansi::bright::GREEN) }
                pub fn yellow()  -> Color { pick(t::ansi::bright::YELLOW,  t::light::ansi::bright::YELLOW,  t::hc::ansi::bright::YELLOW) }
                pub fn blue()    -> Color { pick(t::ansi::bright::BLUE,    t::light::ansi::bright::BLUE,    t::hc::ansi::bright::BLUE) }
                pub fn magenta() -> Color { pick(t::ansi::bright::MAGENTA, t::light::ansi::bright::MAGENTA, t::hc::ansi::bright::MAGENTA) }
                pub fn cyan()    -> Color { pick(t::ansi::bright::CYAN,    t::light::ansi::bright::CYAN,    t::hc::ansi::bright::CYAN) }
                pub fn white()   -> Color { pick(t::ansi::bright::WHITE,   t::light::ansi::bright::WHITE,   t::hc::ansi::bright::WHITE) }
            }
        }

        pub mod search {
            use crate::ui::core::Color;
            use crate::ui::theme::{current, ThemeId, terminal as t};

            #[inline]
            fn pick(dark: Color, light: Color, hc: Color) -> Color {
                match current() {
                    ThemeId::Light        => light,
                    ThemeId::HighContrast => hc,
                    _                     => dark,
                }
            }
            pub fn r#match()         -> Color { pick(t::search::MATCH,         t::light::search::MATCH,         t::hc::search::MATCH) }
            pub fn match_current()   -> Color { pick(t::search::MATCH_CURRENT, t::light::search::MATCH_CURRENT, t::hc::search::MATCH_CURRENT) }
        }
    }
}

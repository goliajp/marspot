//! Semantic tokens — space / radius / typography / elevation.
//!
//! Components reference tokens by name (`space::MD`, `text::body()`).
//! Colours are not here: they are the theme table's (`super::table`),
//! and the text styles and shadows below read the theme in force each
//! time they are asked for.

use crate::ui::core::Length;

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
    use crate::ui::core::Color;
    use crate::ui::theme::table::color;
    use crate::ui::view::{TextSize, TextWeight, TextStyle};

    fn style(size: TextSize, weight: TextWeight, color: Color) -> TextStyle {
        TextStyle { size, weight, color }
    }

    pub fn caption() -> TextStyle { style(TextSize::Caption, TextWeight::Regular, color::fg_muted()) }
    pub fn body() -> TextStyle { style(TextSize::Body, TextWeight::Regular, color::fg()) }
    pub fn header() -> TextStyle { style(TextSize::Header, TextWeight::Bold, color::accent_dim()) }
    pub fn hint() -> TextStyle { style(TextSize::Caption, TextWeight::Regular, color::hint()) }
    pub fn code() -> TextStyle { style(TextSize::Body, TextWeight::Regular, color::accent_dim()) }
    pub fn link() -> TextStyle { style(TextSize::Body, TextWeight::Regular, color::fg_link()) }
    pub fn error() -> TextStyle { style(TextSize::Body, TextWeight::Regular, color::danger()) }
}

/// Semantic elevation shadows — Material-style E0..E3.
pub mod elev {
    use crate::ui::core::Length;
    use crate::ui::theme::table::color;
    use crate::ui::view::Shadow;

    /// How opaque an elevation's shadow is.
    const ALPHA: f64 = 0.55;

    fn shadow(blur: f64, dy: f64) -> Shadow {
        let mut color = color::shadow();
        color.a = ALPHA;
        Shadow { blur: Length::Pt(blur), offset: (Length::Pt(0.0), Length::Pt(dy)), color }
    }

    pub fn e0() -> Shadow { shadow(0.0, 0.0) }
    pub fn e1() -> Shadow { shadow(3.0, 1.0) }
    pub fn e2() -> Shadow { shadow(6.0, 2.0) }
    pub fn e3() -> Shadow { shadow(12.0, 4.0) }
}

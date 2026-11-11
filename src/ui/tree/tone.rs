//! The colours a node can ask for, by what they are for.
//!
//! A tree carries a role and no colour: the role is looked up in the
//! theme in force when the node is painted, so a kept tree follows a
//! change of theme without being described again.  Each role is one of
//! `panel_palette`'s functions; there is no second table.

use crate::ui::components::panel_palette as palette;
use golia_ui_core::Ink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tone {
    Fg,
    FgSecondary,
    FgMuted,
    FgFaint,
    FgDisabled,
    OnAccent,
    Accent,
    AccentHover,
    SegmentBg,
    SegmentHover,
    CardBg,
    CardBorder,
    Separator,
    FrameBg,
    FrameBorder,
    Track,
    RowHover,
    RowSelected,
    KeycapBg,
    Ok,
    Warn,
    Danger,
    SurfaceBase,
}

impl Tone {
    pub const ALL: [Tone; 23] = [
        Tone::Fg,
        Tone::FgSecondary,
        Tone::FgMuted,
        Tone::FgFaint,
        Tone::FgDisabled,
        Tone::OnAccent,
        Tone::Accent,
        Tone::AccentHover,
        Tone::SegmentBg,
        Tone::SegmentHover,
        Tone::CardBg,
        Tone::CardBorder,
        Tone::Separator,
        Tone::FrameBg,
        Tone::FrameBorder,
        Tone::Track,
        Tone::RowHover,
        Tone::RowSelected,
        Tone::KeycapBg,
        Tone::Ok,
        Tone::Warn,
        Tone::Danger,
        Tone::SurfaceBase,
    ];

    /// The colour in the theme in force.
    pub fn rgba(self) -> [f32; 4] {
        match self {
            Tone::Fg => palette::fg(),
            Tone::FgSecondary => palette::fg_sec(),
            Tone::FgMuted => palette::fg_muted(),
            Tone::FgFaint => palette::fg_faint(),
            Tone::FgDisabled => palette::disabled_fg(),
            Tone::OnAccent => palette::on_accent(),
            Tone::Accent => palette::accent(),
            Tone::AccentHover => palette::accent_hover(),
            Tone::SegmentBg => palette::segment_bg(),
            Tone::SegmentHover => palette::hover(palette::segment_bg()),
            Tone::CardBg => palette::card_bg(),
            Tone::CardBorder => palette::card_border(),
            Tone::Separator => palette::separator(),
            Tone::FrameBg => palette::frame_bg(),
            Tone::FrameBorder => palette::frame_border(),
            Tone::Track => palette::track(),
            Tone::RowHover => palette::row_hover(),
            Tone::RowSelected => palette::row_selected(),
            Tone::KeycapBg => palette::keycap_bg(),
            Tone::Ok => palette::ok(),
            Tone::Warn => palette::warn(),
            Tone::Danger => palette::danger(),
            Tone::SurfaceBase => palette::surface_base(),
        }
    }
}

impl From<Tone> for Ink {
    fn from(tone: Tone) -> Ink {
        Ink::Role(tone as u8)
    }
}

/// The colour a node's ink is, now.
pub fn rgba(ink: Ink) -> [f32; 4] {
    match ink {
        Ink::Role(role) => Tone::ALL[role as usize].rgba(),
        Ink::Rgba(c) => [c.r, c.g, c.b, c.a].map(|v| v as f32 / 255.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_build::color::rgba8_of_f32;
    use golia_ui_core::Rgba8;

    #[test]
    fn a_role_comes_back_as_the_tone_it_was_made_from() {
        for (i, tone) in Tone::ALL.iter().enumerate() {
            assert_eq!(*tone as usize, i, "{tone:?} is out of place in the list");
            assert_eq!(rgba(Ink::from(*tone)), tone.rgba());
        }
    }

    #[test]
    fn a_role_is_the_palettes_colour_and_follows_the_theme() {
        assert_eq!(rgba(Tone::Accent.into()), palette::accent());
        assert_eq!(rgba(Tone::FgMuted.into()), palette::fg_muted());
        assert_ne!(rgba(Tone::Fg.into()), rgba(Tone::CardBg.into()));
    }

    #[test]
    fn a_literal_colour_reaches_the_instance_unchanged() {
        for c in [Rgba8::rgba(1, 2, 3, 4), Rgba8::rgba(255, 128, 127, 0), Rgba8::hex(0x1f_73_e6)] {
            assert_eq!(rgba8_of_f32(rgba(Ink::Rgba(c))), c);
        }
    }
}

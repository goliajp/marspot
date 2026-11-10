//! The theme table: every semantic colour the interface draws with,
//! once per theme.
//!
//! A theme is one `Theme`.  The three are statics of the same struct,
//! so they cannot disagree about which colours exist: a field missing
//! from one does not compile.  Which one is in force is the id the
//! terminal palette keeps (`marspot_term::palette`), because the
//! session process needs it too and knows nothing about the interface;
//! this table is indexed by the same id.
//!
//! Nothing outside `ui::theme` and the role functions of
//! `components::panel_palette` reads a field here.  Code that draws per
//! cell takes [`current`] once per frame and passes the reference down.

use crate::ui::core::Color;
use marspot_term::palette::{self, Palette, ThemeId};

macro_rules! theme_table {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        pub struct Theme {
            pub id: ThemeId,
            /// The terminal's own colours under this theme.
            pub term: &'static Palette,
            $($(#[$doc])* pub $name: Color,)*
        }

        /// The current theme's colours by name.
        ///
        /// Not inlined, here and in `rgba`: there are hundreds of call
        /// sites, and the load, the index and the conversion repeated
        /// at each of them cost the binary tens of kilobytes for a
        /// call that happens a few dozen times a frame.
        pub mod color {
            use crate::ui::core::Color;
            $(#[inline(never)] pub fn $name() -> Color { super::current().$name })*
        }

        /// The same, as the unit floats painters take.
        pub mod rgba {
            $(#[inline(never)] pub fn $name() -> [f32; 4] { super::current().$name.to_rgba_f32() })*
        }
    };
}

theme_table! {
    // Text
    fg,
    /// Between `fg` and `fg_muted`: at a panel's density the muted
    /// tone collapses everything but headings into one grey.
    fg_secondary,
    fg_muted,
    fg_disabled,
    fg_inverse,
    fg_link,
    /// A label on the accent or on a status colour used as a fill.
    fg_on_accent,
    hint,

    // Surfaces, lowest first
    bg,
    bg_raised,
    bg_panel,
    bg_hover,
    /// A card on a panel, a popover.
    surface_3,
    /// What lies on a card: an unpicked segment, a key cap, a row
    /// under the pointer.
    surface_4,
    /// The fill of what is selected, on, or the primary action.
    bg_selected,

    // Lines
    border,
    /// A panel's own edge.
    border_strong,
    divider,
    hairline,
    /// Rules running through a plot, behind its bars.
    grid,

    // Accent and status.  The accent says "this one"; the other three
    // say how something is doing and are never used for it.
    accent,
    accent_dim,
    success,
    warn,
    danger,
    /// The same three where they are text: a chip's label, a
    /// percentage, the cross of a danger button at rest.  A fill can
    /// be a mid tone; text on a tint of it cannot.
    success_text,
    warn_text,
    danger_text,

    // Effects.  The opacity of each is the caller's.
    shadow,
    /// Over the rest of the window while a panel is open.
    backdrop,
    /// Over a pane that has stepped back.
    scrim,
    /// What a fill moves towards under the pointer: lighter on a dark
    /// theme, darker on a light one.
    hover_tint,

    // Controls
    knob,
    toggle_off,

    // Main window
    /// Sidebar, header, a pane not in use.
    chrome,
    /// The pane in use.
    canvas,
    /// Opaque, unlike `divider`: seams are drawn in the pass that does
    /// not blend.
    seam,
    focus_ring,
    /// An agent's badge on a pane title.
    agent,

    // Terminal marks
    selection,
    search_hit,
    /// What a glyph on a search hit is drawn in, whatever colour the
    /// program gave it: the hit's background is the theme's, so the
    /// ink that reads on it is too.
    search_hit_fg,
    ime_bg,
    ime_fg,
    cursor_text,
    link,

    // The shell's status banner
    notice_bg,
    notice_fg,
}

const fn c(r: u8, g: u8, b: u8) -> Color {
    Color::rgba(r, g, b, 1.0)
}

/// Today's look, and the default.
pub static DARK: Theme = Theme {
    id: ThemeId::Dark,
    term: &palette::DARK,
    fg: c(232, 238, 248),
    fg_secondary: c(179, 191, 209),
    fg_muted: c(140, 153, 168),
    fg_disabled: c(115, 122, 133),
    fg_inverse: c(20, 22, 28),
    fg_link: c(106, 175, 255),
    fg_on_accent: c(232, 238, 248),
    hint: c(130, 140, 156),
    bg: c(20, 22, 28),
    bg_raised: c(28, 31, 39),
    bg_panel: c(33, 36, 43),
    bg_hover: c(41, 45, 55),
    surface_3: c(44, 47, 56),
    surface_4: c(54, 57, 68),
    bg_selected: c(51, 107, 173),
    border: c(56, 60, 70),
    border_strong: c(82, 87, 102),
    divider: Color::rgba(255, 255, 255, 0.08),
    hairline: Color::rgba(255, 255, 255, 0.18),
    grid: Color::rgba(158, 168, 189, 0.14),
    accent: c(91, 162, 250),
    accent_dim: c(170, 190, 230),
    success: c(80, 190, 110),
    warn: c(230, 200, 80),
    danger: c(220, 60, 60),
    success_text: c(86, 198, 116),
    warn_text: c(230, 200, 80),
    danger_text: c(248, 128, 128),
    shadow: c(0, 0, 0),
    backdrop: c(0, 0, 0),
    scrim: c(0, 0, 0),
    hover_tint: c(255, 255, 255),
    knob: c(232, 238, 248),
    toggle_off: c(54, 57, 68),
    // The window is darker than a panel: a panel floats over it.
    chrome: c(10, 13, 19),
    canvas: c(0, 0, 0),
    seam: c(29, 33, 40),
    focus_ring: c(184, 194, 209),
    agent: c(217, 120, 87),
    selection: c(41, 56, 87),
    search_hit: c(235, 199, 51),
    search_hit_fg: c(20, 22, 28),
    ime_bg: c(26, 33, 46),
    ime_fg: c(204, 219, 235),
    cursor_text: c(235, 235, 235),
    link: c(102, 178, 242),
    notice_bg: Color::rgba(46, 51, 61, 0.85),
    notice_fg: c(247, 242, 232),
};

/// Paper and ink.  The pane in use is the whitest thing in the window
/// and everything else sits a step below it, the way the dark theme's
/// pane in use is its blackest.
pub static LIGHT: Theme = Theme {
    id: ThemeId::Light,
    term: &palette::LIGHT,
    fg: c(28, 32, 40),
    fg_secondary: c(66, 74, 88),
    fg_muted: c(92, 100, 114),
    fg_disabled: c(150, 157, 168),
    fg_inverse: c(255, 255, 255),
    fg_link: c(10, 90, 200),
    fg_on_accent: c(255, 255, 255),
    hint: c(104, 112, 128),
    bg: c(248, 249, 251),
    bg_raised: c(255, 255, 255),
    bg_panel: c(244, 245, 248),
    bg_hover: c(224, 228, 235),
    surface_3: c(255, 255, 255),
    surface_4: c(232, 235, 240),
    bg_selected: c(37, 99, 201),
    border: c(214, 218, 226),
    border_strong: c(176, 183, 194),
    divider: Color::rgba(0, 0, 0, 0.08),
    hairline: Color::rgba(0, 0, 0, 0.14),
    grid: Color::rgba(60, 70, 90, 0.16),
    accent: c(30, 100, 210),
    accent_dim: c(70, 96, 150),
    success: c(22, 122, 52),
    warn: c(146, 96, 0),
    danger: c(196, 38, 38),
    success_text: c(20, 116, 49),
    warn_text: c(140, 92, 0),
    danger_text: c(190, 36, 36),
    // Slate, not black: black at a panel's shadow opacity is a smear
    // on a light ground.
    shadow: c(96, 108, 128),
    backdrop: c(52, 62, 82),
    scrim: c(238, 240, 244),
    hover_tint: c(0, 0, 0),
    knob: c(255, 255, 255),
    toggle_off: c(134, 142, 155),
    chrome: c(238, 240, 244),
    canvas: c(255, 255, 255),
    seam: c(208, 213, 221),
    focus_ring: c(84, 94, 112),
    agent: c(178, 78, 46),
    selection: c(184, 214, 255),
    search_hit: c(255, 214, 10),
    search_hit_fg: c(28, 32, 40),
    ime_bg: c(228, 238, 255),
    ime_fg: c(20, 60, 150),
    cursor_text: c(40, 44, 52),
    link: c(10, 90, 200),
    // A dark notice on a light terminal: a white one would have no
    // edge against the page it lies on.
    notice_bg: Color::rgba(50, 56, 66, 0.85),
    notice_fg: c(247, 244, 238),
};

/// White on black, every text pair at 7:1 or better.  Surfaces are
/// told apart by their borders, not by steps of grey.
pub static CONTRAST: Theme = Theme {
    id: ThemeId::Contrast,
    term: &palette::CONTRAST,
    fg: c(255, 255, 255),
    fg_secondary: c(230, 230, 230),
    fg_muted: c(204, 204, 204),
    fg_disabled: c(150, 150, 150),
    fg_inverse: c(0, 0, 0),
    fg_link: c(140, 210, 255),
    fg_on_accent: c(0, 0, 0),
    hint: c(204, 204, 204),
    bg: c(0, 0, 0),
    bg_raised: c(20, 20, 20),
    bg_panel: c(0, 0, 0),
    bg_hover: c(44, 44, 44),
    surface_3: c(0, 0, 0),
    surface_4: c(40, 40, 40),
    bg_selected: c(102, 204, 255),
    border: c(170, 170, 170),
    border_strong: c(255, 255, 255),
    divider: Color::rgba(255, 255, 255, 0.40),
    hairline: Color::rgba(255, 255, 255, 0.55),
    grid: Color::rgba(255, 255, 255, 0.30),
    accent: c(102, 204, 255),
    accent_dim: c(170, 220, 255),
    success: c(80, 255, 130),
    warn: c(255, 214, 0),
    danger: c(255, 140, 140),
    success_text: c(80, 255, 130),
    warn_text: c(255, 214, 0),
    danger_text: c(255, 140, 140),
    shadow: c(0, 0, 0),
    backdrop: c(0, 0, 0),
    scrim: c(0, 0, 0),
    hover_tint: c(255, 255, 255),
    knob: c(0, 0, 0),
    toggle_off: c(190, 190, 190),
    chrome: c(16, 16, 16),
    canvas: c(0, 0, 0),
    seam: c(130, 130, 130),
    focus_ring: c(255, 255, 255),
    agent: c(255, 170, 130),
    selection: c(0, 64, 160),
    search_hit: c(255, 214, 0),
    search_hit_fg: c(0, 0, 0),
    ime_bg: c(0, 40, 90),
    ime_fg: c(255, 255, 255),
    cursor_text: c(255, 255, 255),
    link: c(140, 210, 255),
    notice_bg: c(255, 255, 255),
    notice_fg: c(0, 0, 0),
};

pub fn of(id: ThemeId) -> &'static Theme {
    match id {
        ThemeId::Dark => &DARK,
        ThemeId::Light => &LIGHT,
        ThemeId::Contrast => &CONTRAST,
    }
}

/// The theme in force: one atomic load and an index.
pub fn current() -> &'static Theme {
    of(palette::current_id())
}

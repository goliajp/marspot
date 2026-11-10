//! The colours drawn inside and behind a pane's grid, in the shape
//! the per-cell paths read them.
//!
//! These are theme tokens like any other; they are here as constant
//! float triples because the code that uses them runs per cell and per
//! glyph, where a colour has to cost nothing to name.

use super::token::color;
use crate::ui::core::Color;

const fn rgb(c: Color) -> (f32, f32, f32) {
    (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0)
}

/// What the frame is cleared to before anything is drawn: the window's
/// own surface, which the sidebar and the header never paint over.
pub(crate) const SIDEBAR_BG_F: (f32, f32, f32) = rgb(color::CHROME);
/// Behind a search hit; the glyph keeps its own colour on top.
pub(crate) const HIGHLIGHT_BG: (f32, f32, f32) = rgb(color::SEARCH_HIT);
/// Behind selected cells.
pub(crate) const SELECTION_BG: (f32, f32, f32) = rgb(color::SELECTION);
/// An IME composition that has not been committed yet.
pub(crate) const IME_PREEDIT_BG: (f32, f32, f32) = rgb(color::IME_BG);
pub(crate) const IME_PREEDIT_FG: (f32, f32, f32) = rgb(color::IME_FG);
/// The cursor: a block in the pane being typed into, an outline elsewhere.
pub(crate) const CURSOR_FG: (f32, f32, f32) = rgb(color::CURSOR_TEXT);
/// A detected link: its underline and its text.
pub(crate) const LINK_UNDERLINE_FG: (f32, f32, f32) = rgb(color::LINK);

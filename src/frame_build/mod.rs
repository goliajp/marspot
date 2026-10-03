//! Laying out a frame: turning panes, chrome and text into the
//! instances a backend draws.  Being moved here out of the Metal
//! renderer a piece at a time; nothing in this module may name a
//! graphics API.

pub mod glyph_resolve;
pub mod text_run;

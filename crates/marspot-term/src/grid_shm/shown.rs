//! What a frame shows of a cell the grid holds otherwise: SGR 8 text
//! goes out as a blank in its colours, SGR 5 goes out steady, an
//! indexed colour a program replaced with OSC 4 goes out as the colour
//! it was replaced with, and under DECSCNM every cell goes out with
//! its reverse video flipped.  The grid keeps all of it as written, so
//! copying still finds concealed text and a reset finds the theme.

use super::{ClusterTableWriter, GridShmWriter};
use crate::grid::{Cell, CellAttrs, Color, ColorKind, PackedStyle};
use crate::terminal::{PaletteOverrides, ScreenModes};

#[derive(Default)]
pub(super) struct Shown {
    /// The overrides in force, `None` while there are none.
    palette: Option<PaletteOverrides>,
    /// [`PaletteOverrides::generation`] of the table above.
    generation: u64,
    /// DECSCNM.  Only twenty-byte frames carry it: an eight-byte
    /// frame's reader takes value 0 as the default whatever it says.
    pub(super) reverse: bool,
}

impl Shown {
    /// Mixed into the frame digest: a palette change has to go out even
    /// when not a cell moved.
    pub(super) fn salt(&self) -> u64 {
        self.generation.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ self.reverse as u64
    }

    fn colour(&self, c: Color) -> Color {
        match (c.kind(), &self.palette) {
            (ColorKind::Indexed(i), Some(p)) => p.get(i).map_or(c, |(r, g, b)| Color::rgb(r, g, b)),
            _ => c,
        }
    }

    /// A twenty-byte frame's cell.
    #[inline]
    pub(super) fn cell(&self, cell: &mut Cell) {
        if cell.attrs.invisible() {
            *cell = Cell { ch: ' ', attrs: cell.attrs };
            cell.attrs._pad[0] = 0;
        }
        cell.attrs.set_blink(false);
        cell.attrs.reverse ^= self.reverse;
        if self.palette.is_some() {
            cell.attrs.fg = self.colour(cell.attrs.fg);
            cell.attrs.bg = self.colour(cell.attrs.bg);
        }
    }

    /// One value of an eight-byte frame's table.
    pub(super) fn style(&self, s: PackedStyle) -> PackedStyle {
        if self.palette.is_none() {
            return s.shown();
        }
        let a = s.unpack();
        PackedStyle::pack(&CellAttrs { fg: self.colour(a.fg), bg: self.colour(a.bg), ..a }).shown()
    }

    /// Whether a table needs [`Self::style`] at all.
    pub(super) fn touches(&self, styles: &[PackedStyle]) -> bool {
        self.palette.is_some() || styles.iter().any(|s| s.has_unshown())
    }
}

impl GridShmWriter {
    /// The palette overrides and DECSCNM the next frame shows.  Cheap
    /// to call on every publish: the table is copied only when it
    /// changed.
    pub fn publish_screen_modes(&mut self, m: &ScreenModes) {
        self.shown.reverse = m.reverse_screen;
        let p = &m.palette;
        if p.generation() != self.shown.generation {
            self.shown.generation = p.generation();
            if p.is_empty() {
                self.shown.palette = None;
            } else if let Some(mine) = &mut self.shown.palette {
                mine.clone_from(p);
            } else {
                self.shown.palette = Some(p.clone());
            }
        }
    }
}

/// Link `c0..=c1` on `row`, less the cells `concealed` names: SGR 8
/// text goes out blank, and its link goes with it.
pub(super) fn push_link(
    t: &mut ClusterTableWriter,
    row: u16,
    c0: u16,
    c1: u16,
    uri: &str,
    concealed: impl Fn(u16) -> bool,
) {
    let mut from = None;
    for c in c0..=c1 {
        match (concealed(c), from) {
            (false, None) => from = Some(c),
            (true, Some(f)) => {
                t.push_link(row, f, c - 1, uri);
                from = None;
            }
            _ => {}
        }
    }
    if let Some(f) = from {
        t.push_link(row, f, c1, uri);
    }
}

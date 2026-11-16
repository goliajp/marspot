//! A digest of what a live row shows, for a reader that caches what it
//! drew of each row and redraws only the rows whose digest moved.

use std::hash::Hasher;

use super::Grid;

impl Grid {
    /// Feed the row a viewport row shows into `h`, when it is one of
    /// the live grid's rows: every cell, the attributes behind its
    /// number, and the text of its clusters.  `false`, with nothing
    /// fed, for a row from the history.
    ///
    /// Equal digests mean equal rows across frames, not just within
    /// one: a number given to another value after a sweep, or a
    /// positional number whose cell changed colour, changes the digest
    /// although the cell's own bits did not.
    pub fn hash_live_row<H: Hasher>(&self, view_offset: u16, viewport_row: u16, h: &mut H) -> bool {
        let Some(cells) = self.live_row_at_view(view_offset, viewport_row) else {
            return false;
        };
        for c in cells {
            h.write_u64(c.bits());
        }
        if self.styles.is_positional() {
            // a number names a position, not a value
            for c in cells.iter().filter(|c| c.style_id() != 0) {
                h.write_u64(self.styles.get(c.style_id()).0);
            }
        } else {
            h.write_u64(self.styles.generation());
        }
        for idx in cells.iter().filter_map(|c| c.cluster_index()) {
            if let Some(text) = self.cluster_text_at(idx) {
                h.write(text.as_bytes());
                h.write_u8(0xFF);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::{Cell, Color, Grid};
    use std::hash::{DefaultHasher, Hasher};

    fn digest(g: &Grid) -> u64 {
        let mut h = DefaultHasher::new();
        assert!(g.hash_live_row(0, 0, &mut h));
        h.finish()
    }

    fn coloured(ch: char, i: u8) -> Cell {
        let mut c = Cell::from(ch);
        c.attrs.fg = Color::indexed(i);
        c
    }

    /// The same number, given to another colour by a table that
    /// started over, is another row.
    #[test]
    fn a_number_that_means_another_colour_is_another_row() {
        let mut g = Grid::new(64, 1);
        g.set_cell(0, 0, coloured('x', 1));
        let red = digest(&g);
        g.clear_all(Cell::default());
        g.set_cell(0, 0, coloured('x', 4));
        assert_eq!(g.live_row_at_view(0, 0).unwrap()[0].style_id(), 1, "the same number again");
        assert_ne!(digest(&g), red);
    }

    /// A positional grid's cell keeps its number when its colour
    /// changes; the digest still has to.
    #[test]
    fn a_positional_cell_that_changes_colour_changes_the_row() {
        let mut g = Grid::new(8, 1);
        for c in 0..8 {
            g.set_cell(c, 0, coloured('x', c as u8 + 1));
        }
        assert!(g.styles().is_positional());
        let before = digest(&g);
        g.set_cell(3, 0, coloured('x', 99));
        assert_ne!(digest(&g), before);
        g.set_cell(3, 0, coloured('x', 4));
        assert_eq!(digest(&g), before);
    }
}

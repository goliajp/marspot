//! Cluster text that arrives beside a frame rather than in a pool.
//!
//! A grid built from another process's published frame has cells whose
//! cluster indices point into a pool it does not have. The text comes
//! separately, keyed by position, and is loaded here.

use super::{CLUSTER_INDEX_BASE, Cell, FLAG_CLUSTER, Grid};

/// The cluster text at `col` in a row fetched by
/// [`Grid::row_clusters_at_view`].
///
/// A linear scan, because a row carries one entry per cluster on it and
/// a row full of them is a few dozen -- a map would cost an allocation
/// per row to save nothing.
pub fn cluster_in_row(row: &[(u16, String)], col: u16) -> Option<&str> {
    row.iter().find(|(c, _)| *c == col).map(|(_, t)| t.as_str())
}

/// What a cluster cell whose text did not arrive reads as.
pub const UNKNOWN_CLUSTER: char = '\u{fffd}';

impl Grid {
    /// Make this grid's clusters exactly `entries`: each `(col, row,
    /// text)` becomes a pool entry and that cell points at it.
    ///
    /// For a grid that mirrors a frame published by another process.
    /// The cells were copied with the publisher's indices, which mean
    /// nothing here, so every cluster cell is first cleared to
    /// [`UNKNOWN_CLUSTER`] -- an index left behind would point at
    /// whatever this pool holds at that slot. Then the entries are
    /// applied.
    ///
    /// The pool is rewritten slot by slot, keeping each `String`'s
    /// capacity, so a frame with the same clusters as the last one
    /// allocates nothing. It never holds more than the latest frame's
    /// entries.
    pub fn mirror_clusters<'a>(&mut self, entries: impl Iterator<Item = (u16, u16, &'a str)>) {
        for c in &mut self.cells {
            if c.is_cluster() {
                c.ch = UNKNOWN_CLUSTER;
                c.attrs._pad[0] &= !FLAG_CLUSTER;
            }
        }
        let mut n = 0usize;
        for (col, row, text) in entries {
            if col >= self.cols || row >= self.rows {
                continue;
            }
            let Some(ch) = u32::try_from(n)
                .ok()
                .and_then(|i| i.checked_add(CLUSTER_INDEX_BASE))
                .and_then(char::from_u32)
            else {
                break;
            };
            match self.clusters.get_mut(n) {
                Some(slot) => {
                    slot.clear();
                    slot.push_str(text);
                }
                None => self.clusters.push(text.to_string()),
            }
            let mut cell: Cell = self.cell(col, row);
            cell.ch = ch;
            cell.attrs._pad[0] |= FLAG_CLUSTER;
            self.set_cell(col, row, cell);
            n += 1;
        }
        self.clusters.truncate(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flagged(index: u32) -> Cell {
        let mut c = Cell { ch: char::from_u32(CLUSTER_INDEX_BASE + index).unwrap(), ..Default::default() };
        c.attrs._pad[0] |= FLAG_CLUSTER;
        c
    }

    #[test]
    fn entries_become_the_text_of_their_cells() {
        let mut g = Grid::new(10, 2);
        g.set_cell(1, 0, flagged(7));
        g.set_cell(4, 1, flagged(3));
        g.mirror_clusters([(1, 0, "\u{1f1ef}\u{1f1f5}"), (4, 1, "e\u{301}")].into_iter());
        assert_eq!(g.cluster_text(&g.cell(1, 0)), Some("\u{1f1ef}\u{1f1f5}"));
        assert_eq!(g.cluster_text(&g.cell(4, 1)), Some("e\u{301}"));
        assert_eq!(g.cluster_pool_len(), 2);
    }

    #[test]
    fn a_cluster_cell_with_no_entry_reads_as_unknown_not_as_another_cluster() {
        let mut g = Grid::new(10, 1);
        g.set_cell(0, 0, flagged(0));
        g.set_cell(5, 0, flagged(1));
        // only the second one's text arrived
        g.mirror_clusters([(5, 0, "e\u{301}")].into_iter());
        let c = g.cell(0, 0);
        assert!(!c.is_cluster());
        assert_eq!(c.ch, UNKNOWN_CLUSTER);
        assert_eq!(g.cluster_text(&g.cell(5, 0)), Some("e\u{301}"));
    }

    #[test]
    fn the_pool_holds_only_the_latest_frame() {
        let mut g = Grid::new(10, 1);
        for _ in 0..1000 {
            g.set_cell(0, 0, flagged(9));
            g.set_cell(1, 0, flagged(9));
            g.mirror_clusters([(0, 0, "a\u{301}"), (1, 0, "o\u{308}")].into_iter());
        }
        assert_eq!(g.cluster_pool_len(), 2);
        g.mirror_clusters(std::iter::empty());
        assert_eq!(g.cluster_pool_len(), 0);
    }

    #[test]
    fn an_entry_outside_the_grid_is_ignored() {
        let mut g = Grid::new(4, 1);
        g.mirror_clusters([(9, 0, "a\u{301}"), (0, 3, "a\u{301}")].into_iter());
        assert_eq!(g.cluster_pool_len(), 0);
    }
}

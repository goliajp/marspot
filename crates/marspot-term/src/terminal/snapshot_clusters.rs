//! The text of the grapheme clusters on screen, in a snapshot.
//!
//! A cluster cell holds an index into its grid's pool, and a snapshot's
//! cell records keep neither the pool nor the mark that says the cell
//! is one: a flag or a family restored from the cell records alone is
//! a private-use codepoint.  So the text follows the snapshot's other
//! sections, by position:
//!
//! ```text
//! magic u32
//! main screen:  count u32, count × (col u16 | row u16 | len u16 | UTF-8)
//! alt screen:   the same; empty unless the alt screen is kept live
//! ```
//!
//! It is a trailer, not a new version: a reader from before it stops
//! at the end of the sections it knows and applies the rest as it
//! always did, and a snapshot from before it simply has no trailer.

use std::io::{self, Cursor, Read};

use super::{CELL_BYTES, Terminal, read_u16, read_u32};
use crate::grid::Grid;

const MAGIC: u32 = 0x4d53_434c; // "MSCL"

/// Can `body` still hold `cols` cells from here?  A length it cannot
/// is not a length.
///
/// A page is written by another process and read back from disk, so
/// `line_cols` is a claim rather than a fact -- and `with_capacity` on
/// a claim turns a malformed page into an abort instead of an error.
/// The Linux CI leg reported exactly that, as SIGABRT, on a runner
/// with less memory to pretend with than a dev box: the legacy probe
/// mis-parses by one byte on purpose, so the next line's length is
/// whatever the cell data happened to spell.
pub(super) fn plausible_cols(cur: &Cursor<&[u8]>, cols: usize) -> io::Result<()> {
    let remaining = cur.get_ref().len().saturating_sub(cur.position() as usize);
    if cols > remaining / CELL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line claims {cols} cells, {remaining} bytes left"),
        ));
    }
    Ok(())
}

/// Append the trailer for `main` and, when it is kept live, `alt`.
pub(super) fn append(out: &mut Vec<u8>, main: &Grid, alt: Option<&Grid>) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    section(out, Some(main));
    section(out, alt);
}

fn section(out: &mut Vec<u8>, grid: Option<&Grid>) {
    let count_at = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let Some(grid) = grid else { return };
    if grid.cluster_pool_len() == 0 {
        return;
    }
    let mut count = 0u32;
    for row in 0..grid.rows() {
        for col in 0..grid.cols() {
            let cell = grid.cell(col, row);
            let Some(text) = grid.cluster_text(&cell).filter(|t| t.len() <= u16::MAX as usize) else {
                continue;
            };
            out.extend_from_slice(&col.to_le_bytes());
            out.extend_from_slice(&row.to_le_bytes());
            out.extend_from_slice(&(text.len() as u16).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
            count += 1;
        }
    }
    out[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
}

type Entries = Vec<(u16, u16, String)>;

fn read_section(cur: &mut Cursor<&[u8]>) -> io::Result<Entries> {
    let count = read_u32(cur)?;
    let mut entries = Vec::new();
    for _ in 0..count {
        let (col, row, len) = (read_u16(cur)?, read_u16(cur)?, read_u16(cur)?);
        let mut text = vec![0u8; len as usize];
        cur.read_exact(&mut text)?;
        let text = String::from_utf8(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        entries.push((col, row, text));
    }
    Ok(entries)
}

/// The trailer at `cur`, when there is one and it reads whole.
fn read(cur: &mut Cursor<&[u8]>) -> Option<(Entries, Entries)> {
    if read_u32(cur).ok()? != MAGIC {
        return None;
    }
    Some((read_section(cur).ok()?, read_section(cur).ok()?))
}

fn load(grid: &mut Grid, entries: &Entries) {
    grid.mirror_clusters(entries.iter().map(|(c, r, t)| (*c, *r, t.as_str())));
}

impl Terminal {
    /// Give the screens just restored from a snapshot the cluster text
    /// that follows it at `cur`.  `alt_live` says the snapshot kept
    /// the alt screen, which is then the grid in front.
    pub(super) fn apply_snapshot_clusters(&mut self, cur: &mut Cursor<&[u8]>, alt_live: bool) {
        let Some((main, alt)) = read(cur) else { return };
        match self.saved_main.as_mut().filter(|_| alt_live) {
            Some(saved) => {
                load(&mut saved.grid, &main);
                load(&mut self.grid, &alt);
            }
            None => load(&mut self.grid, &main),
        }
    }

    /// Take the cluster text `published` holds for the screen in
    /// front, where this screen has a cluster's index and not its
    /// text: the image before this one published it beside its last
    /// frame, and wrote a snapshot from before snapshots carried it.
    /// Nothing is taken when this screen already has cluster text.
    pub fn adopt_published_clusters<'a>(
        &mut self,
        published: impl Iterator<Item = (u16, u16, &'a str)>,
    ) {
        if self.grid.cluster_pool_len() > 0 {
            return;
        }
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        let entries: Entries = published
            .filter(|(col, row, _)| {
                // only where the snapshot left a bare pool index
                *col < cols
                    && *row < rows
                    && self.grid.cell(*col, *row).ch as u32 >= crate::grid::CLUSTER_INDEX_BASE
            })
            .map(|(c, r, t)| (c, r, t.to_string()))
            .collect();
        if !entries.is_empty() {
            load(&mut self.grid, &entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAG: &str = "\u{1f1ef}\u{1f1f5}";
    const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";

    fn clusters(grid: &Grid) -> Vec<String> {
        let mut out = Vec::new();
        for row in 0..grid.rows() {
            for col in 0..grid.cols() {
                let cell = grid.cell(col, row);
                if cell.is_cluster() {
                    out.push(grid.cluster_text(&cell).unwrap_or("?").to_string());
                }
            }
        }
        out
    }

    #[test]
    fn the_clusters_on_screen_come_back_from_a_snapshot() {
        let mut term = Terminal::new(20, 3);
        term.feed(format!("{FLAG} a {FAMILY}").as_bytes());
        for body in [term.serialize_snapshot_live(), term.serialize_snapshot()] {
            let mut back = Terminal::new(20, 3);
            back.apply_snapshot(&body).unwrap();
            assert_eq!(clusters(back.grid()), [FLAG, FAMILY]);
            // and the next cluster written lands beside them
            back.feed(format!(" {FLAG}").as_bytes());
            assert_eq!(clusters(back.grid()), [FLAG, FAMILY, FLAG]);
        }
    }

    #[test]
    fn both_screens_keep_their_clusters_across_a_live_snapshot() {
        let mut term = Terminal::new(20, 3);
        term.feed(format!("{FLAG}\x1b[?1049h{FAMILY}").as_bytes());
        let mut back = Terminal::new(20, 3);
        back.apply_snapshot(&term.serialize_snapshot_live()).unwrap();
        assert_eq!(clusters(back.grid()), [FAMILY], "the alt screen");
        back.feed(b"\x1b[?1049l");
        assert_eq!(clusters(back.grid()), [FLAG], "the main screen under it");
    }

    /// A snapshot as an image from before the trailer wrote it.
    fn without_trailer(term: &Terminal) -> Vec<u8> {
        let mut body = term.serialize_snapshot_live();
        let mut trailer = Vec::new();
        append(&mut trailer, &term.grid, None);
        body.truncate(body.len() - trailer.len());
        body
    }

    #[test]
    fn a_snapshot_without_the_trailer_still_loads() {
        let mut term = Terminal::new(20, 3);
        term.feed(format!("x{FLAG}y").as_bytes());
        let mut back = Terminal::new(20, 3);
        back.apply_snapshot(&without_trailer(&term)).unwrap();
        assert_eq!(back.grid().cell(0, 0).ch, 'x');
        assert!(clusters(back.grid()).is_empty());
    }

    #[test]
    fn text_published_by_the_image_before_fills_a_snapshot_without_it() {
        let mut term = Terminal::new(20, 3);
        term.feed(format!("x{FLAG}y").as_bytes());
        let mut back = Terminal::new(20, 3);
        back.apply_snapshot(&without_trailer(&term)).unwrap();
        // one entry where the cluster is, one where an ordinary cell is
        back.adopt_published_clusters([(1, 0, FLAG), (0, 0, FAMILY)].into_iter());
        assert_eq!(clusters(back.grid()), [FLAG]);
        assert_eq!(back.grid().cell(0, 0).ch, 'x', "an ordinary cell was taken for a cluster");
    }
}

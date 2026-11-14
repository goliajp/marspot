//! The text of the grapheme clusters a snapshot's cells stand for.
//!
//! A cluster cell on screen holds an index into its grid's pool, and a
//! row of history holds the cluster's first codepoint with the text
//! kept beside the row.  A snapshot's cell records keep neither the
//! pool, nor the mark that says a cell is a cluster, nor a history
//! row's text: restored from the cell records alone, a flag on screen
//! is a private-use codepoint and a flag in history is half a flag.
//! So the text follows the snapshot's other sections:
//!
//! ```text
//! magic u32
//! main screen:   count u32, count x (col u16 | row u16 | len u16 | UTF-8)
//! alt screen:    the same; empty unless the alt screen is kept live
//! main history:  count u32, count x (line u32 | col u16 | len u16 | UTF-8)
//! alt history:   the same
//! ```
//!
//! A history `line` counts from the first line of the section of the
//! snapshot it belongs to: the main scrollback tail, and the alt
//! screen's ring, which for a folded alt screen is followed by its
//! visible rows.
//!
//! It is a trailer, not a new version: a reader from before it stops
//! at the end of the sections it knows and applies the rest as it
//! always did, and a snapshot from before it simply has no trailer.

use std::io::{self, Cursor, Read};

use super::{CELL_BYTES, SNAPSHOT_SCROLLBACK_LINE_CAP, Terminal, read_u16, read_u32};
use crate::grid::{CLUSTER_INDEX_BASE, Cell, Grid};

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

/// Append the trailer.  `alt` is the alt screen when the terminal is
/// in it, `live` whether the snapshot keeps it as a screen or folds it
/// into history, and `tail_cap` the cap the main scrollback tail was
/// written under.
pub(super) fn append(out: &mut Vec<u8>, main: &Grid, alt: Option<&Grid>, live: bool, tail_cap: usize) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    screen(out, Some(main));
    screen(out, alt.filter(|_| live));
    history(out, main, tail_cap, false);
    match alt {
        Some(alt) => history(out, alt, SNAPSHOT_SCROLLBACK_LINE_CAP, !live),
        None => out.extend_from_slice(&0u32.to_le_bytes()),
    }
}

/// Write a counted section whose entries `fill` appends, returning
/// how many.
fn counted(out: &mut Vec<u8>, fill: impl FnOnce(&mut Vec<u8>) -> u32) {
    let count_at = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let count = fill(out);
    out[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
}

fn text_of(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u16).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn fits(text: &str) -> bool {
    text.len() <= u16::MAX as usize
}

fn screen(out: &mut Vec<u8>, grid: Option<&Grid>) {
    counted(out, |out| {
        let Some(grid) = grid.filter(|g| g.cluster_pool_len() > 0) else { return 0 };
        let mut count = 0;
        for row in 0..grid.rows() {
            for col in 0..grid.cols() {
                let cell = grid.cell(col, row);
                let Some(text) = grid.cluster_text(&cell).filter(|t| fits(t)) else { continue };
                out.extend_from_slice(&col.to_le_bytes());
                out.extend_from_slice(&row.to_le_bytes());
                text_of(out, text);
                count += 1;
            }
        }
        count
    });
}

/// The clusters of the newest `cap` lines of `grid`'s history and,
/// with `and_screen`, of the visible rows after them.
fn history(out: &mut Vec<u8>, grid: &Grid, cap: usize, and_screen: bool) {
    counted(out, |out| {
        let sb_len = grid.scrollback_len();
        let take = sb_len.min(cap);
        let mut count = 0;
        let mut put = |out: &mut Vec<u8>, line: usize, col: u16, text: &str| {
            if fits(text) {
                out.extend_from_slice(&(line as u32).to_le_bytes());
                out.extend_from_slice(&col.to_le_bytes());
                text_of(out, text);
                count += 1;
            }
        };
        let mut row = Vec::new();
        for i in 0..take {
            grid.scrollback_clusters(sb_len - take + i, &mut row);
            for (col, text) in &row {
                put(out, i, *col, text);
            }
        }
        if and_screen && grid.cluster_pool_len() > 0 {
            for r in 0..grid.rows() {
                for col in 0..grid.cols() {
                    if let Some(text) = grid.cluster_text(&grid.cell(col, r)) {
                        put(out, take + r as usize, col, text);
                    }
                }
            }
        }
        count
    });
}

type Screen = Vec<(u16, u16, String)>;
type History = std::collections::HashMap<u32, Vec<(u16, String)>>;

/// A snapshot's trailer, read.  Empty for a snapshot without one.
#[derive(Default)]
pub(super) struct Trailer {
    main: Screen,
    alt: Screen,
    main_history: History,
    alt_history: History,
}

fn read_text(cur: &mut Cursor<&[u8]>) -> io::Result<String> {
    let mut text = vec![0u8; read_u16(cur)? as usize];
    cur.read_exact(&mut text)?;
    String::from_utf8(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_screen(cur: &mut Cursor<&[u8]>) -> io::Result<Screen> {
    let mut entries = Vec::new();
    for _ in 0..read_u32(cur)? {
        let (col, row) = (read_u16(cur)?, read_u16(cur)?);
        entries.push((col, row, read_text(cur)?));
    }
    Ok(entries)
}

fn read_history(cur: &mut Cursor<&[u8]>) -> io::Result<History> {
    let mut lines = History::new();
    for _ in 0..read_u32(cur)? {
        let (line, col) = (read_u32(cur)?, read_u16(cur)?);
        lines.entry(line).or_default().push((col, read_text(cur)?));
    }
    Ok(lines)
}

impl Trailer {
    /// The trailer at `cur`; nothing of one that does not read whole.
    pub(super) fn read(cur: &mut Cursor<&[u8]>) -> Self {
        let mut all = || -> io::Result<Self> {
            if read_u32(cur)? != MAGIC {
                return Ok(Self::default());
            }
            Ok(Self {
                main: read_screen(cur)?,
                alt: read_screen(cur)?,
                main_history: read_history(cur)?,
                alt_history: read_history(cur)?,
            })
        };
        all().unwrap_or_default()
    }

    /// Push the alt screen's ring into `alt`, each line with its
    /// clusters.
    pub(super) fn push_alt_ring(&self, alt: &mut Grid, ring: &[(Vec<Cell>, bool)]) {
        for (i, (line, wrapped)) in ring.iter().enumerate() {
            push_line(alt, &self.alt_history, i, line.clone(), *wrapped);
        }
    }
}

/// Push history line `i` of its section with the clusters `from` has
/// for it.  A cell that came from a screen holds a pool index; in
/// history it holds the cluster's first codepoint.
fn push_line(grid: &mut Grid, from: &History, i: usize, mut line: Vec<Cell>, wrapped: bool) {
    let clusters = from.get(&(i as u32)).cloned().unwrap_or_default();
    for (col, text) in &clusters {
        if let Some(cell) = line.get_mut(*col as usize).filter(|c| c.ch as u32 >= CLUSTER_INDEX_BASE) {
            cell.ch = crate::grapheme::cluster_first_codepoint(text);
        }
    }
    grid.push_historic_line_with_clusters(&line, wrapped, clusters);
}

fn load(grid: &mut Grid, entries: &Screen) {
    grid.mirror_clusters(entries.iter().map(|(c, r, t)| (*c, *r, t.as_str())));
}

impl Terminal {
    /// Put a snapshot's history back, in arrival order, so the ring
    /// has the shape it had.
    ///
    /// The main tail: from v3 the snapshot says each line's logical
    /// index (`start_idx + i`), and only the lines the on-disk
    /// scrollback does not have are pushed, which dedups against the
    /// file and refills what its writer had not flushed.  Before v3
    /// there is no index, and the tail is replayed only onto an empty
    /// scrollback.
    ///
    /// The folded alt screen (`fold`): its ring and final screen
    /// existed only in RAM, so there is nothing to dedup against;
    /// they are appended, and a file-backed scrollback keeps them.
    pub(super) fn replay_history(
        &mut self,
        snapshot_v: u32,
        start_idx: u64,
        tail: Vec<(Vec<Cell>, bool)>,
        fold: Vec<(Vec<Cell>, bool)>,
        trailer: &Trailer,
    ) {
        let on_disk = self.grid.scrollback_len() as u64;
        let skip = if snapshot_v >= 3 {
            on_disk.saturating_sub(start_idx) as usize
        } else if on_disk == 0 {
            0
        } else {
            tail.len()
        };
        for (i, (line, wrapped)) in tail.into_iter().enumerate().skip(skip) {
            push_line(&mut self.grid, &trailer.main_history, i, line, wrapped);
        }
        for (i, (line, wrapped)) in fold.into_iter().enumerate() {
            push_line(&mut self.grid, &trailer.alt_history, i, line, wrapped);
        }
    }

    /// Give the screens just restored from a snapshot their cluster
    /// text.  `alt_live` says the snapshot kept the alt screen, which
    /// is then the grid in front.
    pub(super) fn apply_snapshot_clusters(&mut self, trailer: &Trailer, alt_live: bool) {
        match self.saved_main.as_mut().filter(|_| alt_live) {
            Some(saved) => {
                load(&mut saved.grid, &trailer.main);
                load(&mut self.grid, &trailer.alt);
            }
            None => load(&mut self.grid, &trailer.main),
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
        let entries: Screen = published
            .filter(|(col, row, _)| {
                // only where the snapshot left a bare pool index
                *col < cols && *row < rows && self.grid.cell(*col, *row).ch as u32 >= CLUSTER_INDEX_BASE
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
        append(&mut trailer, &term.grid, None, true, SNAPSHOT_SCROLLBACK_LINE_CAP);
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

    /// What scrolling back over the whole history shows, cluster text
    /// in place: oldest line first.
    fn history_text(grid: &Grid) -> Vec<String> {
        let mut row = Vec::new();
        (0..grid.scrollback_len())
            .map(|i| {
                grid.scrollback_clusters(i, &mut row);
                let line = grid.scrollback_line(i).unwrap();
                let mut text = String::new();
                for (col, cell) in line.iter().enumerate() {
                    match row.iter().find(|(c, _)| *c as usize == col) {
                        Some((_, t)) => text.push_str(t),
                        None if cell.ch != '\0' => text.push(cell.ch),
                        None => {}
                    }
                }
                text.trim_end().to_string()
            })
            .collect()
    }

    fn scrolled(prefix: &str) -> Terminal {
        let mut term = Terminal::new(20, 3);
        term.feed(prefix.as_bytes());
        for i in 0..6 {
            term.feed(format!("{i} {FLAG} {FAMILY}\r\n").as_bytes());
        }
        term
    }

    #[test]
    fn clusters_in_history_come_back_from_a_snapshot() {
        let term = scrolled("");
        let before = history_text(term.grid());
        assert!(before.len() >= 4 && before[0] == format!("0 {FLAG} {FAMILY}"), "{before:?}");
        for body in [term.serialize_snapshot_live(), term.serialize_snapshot()] {
            let mut back = Terminal::new(20, 3);
            back.apply_snapshot(&body).unwrap();
            assert_eq!(history_text(back.grid()), before);
        }
    }

    #[test]
    fn clusters_in_the_alt_screens_history_come_back_from_a_live_snapshot() {
        let term = scrolled("\x1b[?1049h");
        let before = history_text(term.grid());
        assert!(before.len() >= 4, "the alt screen kept no history: {before:?}");
        let mut back = Terminal::new(20, 3);
        back.apply_snapshot(&term.serialize_snapshot_live()).unwrap();
        assert_eq!(history_text(back.grid()), before);
    }

    /// A session that ends inside a full-screen program: its alt
    /// screen, history and visible rows, becomes history of the shell
    /// that follows.
    #[test]
    fn a_folded_alt_screen_keeps_its_clusters_as_history() {
        let term = scrolled("\x1b[?1049h");
        let mut back = Terminal::new(20, 3);
        back.apply_snapshot(&term.serialize_snapshot()).unwrap();
        let folded = history_text(back.grid());
        let want: Vec<String> = (0..6).map(|i| format!("{i} {FLAG} {FAMILY}")).collect();
        assert_eq!(&folded[..6], &want[..], "{folded:?}");
    }
}

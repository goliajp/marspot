//! OSC 8 hyperlinks: the pool a grid's cells point into, and the
//! links of rows that have scrolled into history.
//!
//! The same split as clusters, for the same reason: a cell pays for a
//! number, not for a URI.  A cell on screen holds a link id (see
//! [`super::CellAttrs::link_id`]) and the pool holds the URI.  A row
//! leaving the screen has its ids cleared and its links collected by
//! column range, so a history row is plain cells, byte for byte what a
//! binary from before links writes and reads, and its links live beside
//! it: in the file's notes for a file-backed pane, in `sb_links` for one
//! kept in RAM.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};

use super::{Cell, Grid};

/// One link on a row: first column, last column (inclusive), URI.
pub type RowLink = (u16, u16, String);

/// The most links a grid's pool holds at once: every id a cell can
/// carry, 0 excepted.  Past it a new link is not given an id at all --
/// its text is written, it is just not a link.  Ids are never reused
/// while a cell may still carry one, so a full pool cannot make a cell
/// point at another link's URI.
pub const LINK_POOL_MAX: usize = u16::MAX as usize;

/// Sweep the pool once it holds this many more than the last sweep
/// left: a screen cannot point at more links than it has cells, so the
/// pool stays a small multiple of what the screen shows.
const SWEEP_AT: usize = 1024;

/// The most URI bytes a row filed into history keeps; links past it on
/// the row are filed as plain text.  A program printing a row of
/// distinct maximal URIs must not make each line of history hold
/// hundreds of kilobytes.
pub const ROW_LINK_BYTES: usize = 4096;

/// The URIs a grid's cells point at, id `i + 1` at index `i`.
#[derive(Default)]
pub(super) struct LinkPool {
    uris: Vec<String>,
    /// By a keyed hash of the URI, so the same link written in several
    /// pieces -- a program redrawing a line, or a URI split across a
    /// wrap -- takes one id rather than one per piece.  A collision
    /// only costs a second entry for the same text.
    index: HashMap<u64, u16>,
    hasher: RandomState,
    /// What the last sweep left.
    kept: usize,
    /// Moves whenever an id's meaning can have changed.
    generation: u64,
}

impl LinkPool {
    fn key(&self, uri: &str) -> u64 {
        self.hasher.hash_one(uri)
    }
}

impl Grid {
    /// The id cells of link `uri` carry, adding it to the pool if it is
    /// not there.  `None` when the pool is full.
    pub fn link_id_for(&mut self, uri: &str) -> Option<u16> {
        let key = self.links.key(uri);
        if let Some(&id) = self.links.index.get(&key)
            && self.links.uris.get(id as usize - 1).is_some_and(|u| u == uri)
        {
            return Some(id);
        }
        if self.links.uris.len() >= LINK_POOL_MAX {
            return None;
        }
        self.links.uris.push(uri.to_string());
        let id = self.links.uris.len() as u16;
        self.links.index.insert(key, id);
        self.links.generation += 1;
        Some(id)
    }

    /// The URI of link `id`, or `None` for 0 and for an id the pool
    /// does not hold.
    pub fn link_uri(&self, id: u16) -> Option<&str> {
        (id as usize).checked_sub(1).and_then(|i| self.links.uris.get(i)).map(String::as_str)
    }

    /// How many links the pool is holding.
    pub fn link_pool_len(&self) -> usize {
        self.links.uris.len()
    }

    /// Changes whenever a link id may have come to mean another URI.
    pub fn link_generation(&self) -> u64 {
        self.links.generation
    }

    /// Whether the pool has grown enough since the last sweep for one
    /// to be worth its pass over the cells.
    pub fn link_sweep_due(&self) -> bool {
        self.links.uris.len() >= self.links.kept + SWEEP_AT
    }

    /// Whether live row `row` may hold a cell carrying a link.  False is
    /// certain; true is a bound kept by the row's marks.
    #[inline(always)]
    pub fn row_may_link(&self, row: u16) -> bool {
        !self.links.uris.is_empty() && self.marks[self.phys_row(row)].links
    }

    /// Clear the mark of each row no cell of which carries a link any
    /// more, and empty the pool when no row does, so a grid whose links
    /// have all been overwritten or scrolled off is back on the paths
    /// that know it has none.  The caller must hold no id: with a link
    /// open, an id handed out may not be on any cell yet.
    ///
    /// Looks only at the rows whose marks say they may hold a link.
    pub(crate) fn settle_links(&mut self) {
        if self.links.uris.is_empty() {
            return;
        }
        let cols = self.cols as usize;
        let mut any = false;
        for (pr, mark) in self.marks.iter_mut().enumerate() {
            if mark.links {
                mark.links = self.cells[pr * cols..(pr + 1) * cols].iter().any(|c| c.link_id() != 0);
                any |= mark.links;
            }
        }
        if !any {
            self.links.uris.clear();
            self.links.index.clear();
            self.links.kept = 0;
            self.links.generation += 1;
        }
    }

    /// Drop pool entries no cell points at, and renumber the rest.
    ///
    /// A cell whose id the pool does not hold afterwards is cleared, so
    /// it reads as no link rather than as whichever link took its slot.
    pub fn sweep_links(&mut self) {
        let n = self.links.uris.len();
        let mut remap = vec![0u16; n + 1];
        for c in &self.cells {
            let id = c.link_id() as usize;
            if id != 0 && id <= n {
                remap[id] = 1;
            }
        }
        let mut next = 0u16;
        let mut kept = Vec::new();
        for (i, slot) in remap.iter_mut().enumerate().skip(1) {
            if *slot != 0 {
                next += 1;
                *slot = next;
                kept.push(std::mem::take(&mut self.links.uris[i - 1]));
            }
        }
        for c in &mut self.cells {
            let id = c.link_id() as usize;
            if id != 0 {
                *c = c.with_link(remap.get(id).copied().unwrap_or(0));
            }
        }
        self.links.uris = kept;
        self.links.index.clear();
        for (i, uri) in self.links.uris.iter().enumerate() {
            let key = self.links.hasher.hash_one(uri.as_str());
            self.links.index.entry(key).or_insert(i as u16 + 1);
        }
        self.links.kept = self.links.uris.len();
        self.links.generation += 1;
    }

    /// Clear the link ids of storage row `start..start + cols` and hand
    /// back its links by column range.  Called just before the row is
    /// filed into history, where an id would point into a pool that
    /// sweeps without it.  Links past [`ROW_LINK_BYTES`] of URI on the
    /// row are dropped.
    fn degrade_links_in_row(&mut self, start: usize, cols: usize) -> Vec<RowLink> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut run: Option<(u16, usize)> = None;
        for i in start..=start + cols {
            let id = if i < start + cols { self.cells[i].link_id() } else { 0 };
            if run.is_some_and(|(r, _)| r != id) {
                let (r, from) = run.take().expect("checked");
                if let Some(uri) = self.link_uri(r)
                    && bytes + uri.len() <= ROW_LINK_BYTES
                {
                    bytes += uri.len();
                    out.push(((from - start) as u16, (i - 1 - start) as u16, uri.to_string()));
                }
            }
            if id != 0 {
                if run.is_none() {
                    run = Some((id, i));
                }
                self.cells[i] = self.cells[i].with_link(0);
            }
        }
        out
    }

    /// Clear a row's cluster indices and link ids on its way into
    /// history, handing back what they said.  One row in millions has
    /// either, so the tests are made here and the calls only for it.
    #[inline(always)]
    pub(super) fn degrade_row_extras(&mut self, start: usize, cols: usize) -> (Vec<(u16, String)>, Vec<RowLink>) {
        let clusters = if self.clusters.is_empty() { Vec::new() } else { self.degrade_clusters_in_row(start, cols) };
        let links = if self.links.uris.is_empty() || !self.marks[start / cols].links {
            Vec::new()
        } else {
            self.degrade_links_in_row(start, cols)
        };
        (clusters, links)
    }

    /// Keep what [`Self::degrade_row_extras`] handed back, for the line
    /// just pushed into scrollback.
    #[inline(always)]
    pub(super) fn keep_row_extras(&mut self, clusters: Vec<(u16, String)>, links: Vec<RowLink>) {
        if !clusters.is_empty() || !self.scrollback.keeps_cluster_text() {
            self.keep_row_clusters(clusters);
        }
        if !links.is_empty() || !self.scrollback.keeps_link_text() {
            self.keep_row_links(links);
        }
    }

    /// Keep the links of the line just pushed into scrollback, where
    /// this grid's scrollback keeps them.  Must be called after the
    /// line itself is pushed: the file addresses a note by the line it
    /// follows.  For the in-RAM variant it must be called for every
    /// line, empty or not, so `sb_links` stays parallel to the ring.
    fn keep_row_links(&mut self, links: Vec<RowLink>) {
        if !self.scrollback.keeps_link_text() {
            self.sb_links.push_back(links);
        } else if !links.is_empty() {
            self.scrollback.link_last_line(&links);
        }
    }

    /// Trim the in-RAM mirrors of history to what the ring holds.
    pub(super) fn trim_extras_mirrors(&mut self) {
        let len = self.scrollback.len();
        while self.sb_clusters.len() > len {
            self.sb_clusters.pop_front();
        }
        while self.sb_links.len() > len {
            self.sb_links.pop_front();
        }
    }

    /// The links on the row a viewport row is showing, into `out`,
    /// which is cleared first.  See [`Self::each_link_at_view`].
    pub fn row_links_at_view(&self, view_offset: u16, viewport_row: u16, out: &mut Vec<RowLink>) -> bool {
        out.clear();
        self.each_link_at_view(view_offset, viewport_row, |c0, c1, uri| out.push((c0, c1, uri.to_string())));
        !out.is_empty()
    }

    /// Each link on the row a viewport row is showing, as (first
    /// column, last column, URI), allocating nothing.  A live row's come
    /// from its cells and the pool, a history row's from where history
    /// keeps them; asked once per row, like
    /// [`Self::row_clusters_at_view`] and with the same index arithmetic.
    pub fn each_link_at_view(&self, view_offset: u16, viewport_row: u16, mut f: impl FnMut(u16, u16, &str)) {
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            let row = (rows - 1 - abs) as u16;
            if !self.row_may_link(row) {
                return;
            }
            let start = self.phys_row(row) * self.cols as usize;
            let cells = &self.cells[start..start + self.cols as usize];
            let mut c = 0usize;
            while c < cells.len() {
                let id = cells[c].link_id();
                let from = c;
                while c < cells.len() && cells[c].link_id() == id {
                    c += 1;
                }
                if id != 0
                    && let Some(uri) = self.link_uri(id)
                {
                    f(from as u16, (c - 1) as u16, uri);
                }
            }
            return;
        }
        let from_end = abs - rows;
        let sb_len = self.scrollback_len();
        if from_end >= sb_len {
            return;
        }
        let line = sb_len - 1 - from_end;
        if self.scrollback.keeps_link_text() {
            self.scrollback.each_link_at(line, f);
        } else if let Some(row) = self.sb_links.get(line) {
            for (c0, c1, uri) in row {
                f(*c0, *c1, uri);
            }
        }
    }

    /// The links of scrollback line `line`, 0 = oldest, into `out`,
    /// which is cleared first.
    pub fn scrollback_links(&self, line: usize, out: &mut Vec<RowLink>) {
        out.clear();
        if self.scrollback.keeps_link_text() {
            self.scrollback.links_at(line, out);
        } else if let Some(row) = self.sb_links.get(line) {
            out.extend(row.iter().cloned());
        }
    }

    /// The link covering (`col`, `viewport_row`) at `view_offset`, as
    /// (first column, last column, URI).  For a click: it reads the
    /// whole row, so it is not for a per-cell loop.
    pub fn link_span_at_view(&self, view_offset: u16, col: u16, viewport_row: u16) -> Option<RowLink> {
        let mut row = Vec::new();
        self.row_links_at_view(view_offset, viewport_row, &mut row);
        row.into_iter().find(|(c0, c1, _)| (*c0..=*c1).contains(&col))
    }

    /// Make this grid's links exactly `entries`, each `(row, first col,
    /// last col, URI)` on the live screen.
    ///
    /// For a grid mirroring a frame another process published: the
    /// frame's cells carry no ids, and the links arrive beside it.
    /// Every id in the grid is cleared first, so a cell the entries do
    /// not name is not a link.
    pub fn mirror_links<'a>(&mut self, entries: impl Iterator<Item = (u16, u16, u16, &'a str)>) {
        let cols = self.cols as usize;
        // only a row whose marks say so can hold an id
        for (pr, mark) in self.marks.iter_mut().enumerate() {
            if mark.links {
                for c in &mut self.cells[pr * cols..(pr + 1) * cols] {
                    *c = c.with_link(0);
                }
                mark.links = false;
            }
        }
        self.links.index.clear();
        self.links.kept = 0;
        self.links.generation += 1;
        // slot by slot, keeping each `String`'s capacity: a frame with
        // the links of the last one allocates nothing
        let mut n = 0usize;
        for (row, c0, c1, uri) in entries {
            if row >= self.rows || c0 > c1 || c1 >= self.cols || n >= LINK_POOL_MAX {
                continue;
            }
            match self.links.uris.get_mut(n) {
                Some(slot) => {
                    slot.clear();
                    slot.push_str(uri);
                }
                None => self.links.uris.push(uri.to_string()),
            }
            n += 1;
            let pr = self.phys_row(row);
            self.marks[pr].links = true;
            let start = pr * cols;
            for c in &mut self.cells[start + c0 as usize..=start + c1 as usize] {
                *c = c.with_link(n as u16);
            }
        }
        self.links.uris.truncate(n);
    }
}

/// Clear the link ids of `row`, for cells on their way into history by
/// a path that carries `Cell`s rather than the grid's own rows.
pub fn strip_links(row: &mut [Cell]) {
    for c in row {
        c.attrs.set_link_id(0);
    }
}

/// A viewport row's clusters and links, fetched together: the readers
/// that need one need the other in the same loop.  Kept by the caller
/// and reused from row to row.
#[derive(Default, Debug)]
pub struct RowExtras {
    pub clusters: Vec<(u16, String)>,
    pub links: Vec<RowLink>,
}

impl Grid {
    /// Both halves of [`RowExtras`] for one viewport row.  True when the
    /// row has either.
    pub fn row_extras_at_view(&self, view_offset: u16, viewport_row: u16, out: &mut RowExtras) -> bool {
        let c = self.row_clusters_at_view(view_offset, viewport_row, &mut out.clusters);
        let l = self.row_links_at_view(view_offset, viewport_row, &mut out.links);
        c || l
    }
}

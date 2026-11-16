//! The pool of grapheme clusters a grid's cells point into.

use super::{CLUSTER_INDEX_BASE, Cell, CellAttrs, FLAG_CLUSTER, Grid};

impl Grid {
    /// Put a multi-codepoint cluster in the pool and hand back the
    /// cell that points at it.
    ///
    /// The caller writes that cell wherever the cluster goes.  Callers
    /// with a single codepoint do not come here — they keep writing
    /// `ch` directly, and their cells are byte-for-byte what they
    /// always were.
    pub fn cluster_cell(&mut self, cluster: &str, attrs: CellAttrs) -> Cell {
        self.cluster_cell_reusing(None, cluster, attrs)
    }

    /// The same, but told which entry the cell it is replacing used.
    ///
    /// A cluster is written again on every codepoint that joins it —
    /// `e`, then `é`, then `é` with a second mark — and each write
    /// used to push a new entry, so a seven-codepoint cluster
    /// allocated six times and left five for the sweep.  Measured on a
    /// stream that is 36% clusters: 47.9 MB/s before the pool existed,
    /// 41.8 with a push per join.
    ///
    /// When the previous entry is the newest one, nobody else can be
    /// pointing at it, so it is rewritten in place and the `String`
    /// keeps its capacity.
    pub fn cluster_cell_reusing(
        &mut self,
        prev: Option<u32>,
        cluster: &str,
        attrs: CellAttrs,
    ) -> Cell {
        debug_assert!(cluster.chars().count() > 1, "one codepoint needs no pool");
        if let Some(i) = prev
            && (i as usize) + 1 == self.clusters.len()
        {
            let slot = &mut self.clusters[i as usize];
            slot.clear();
            slot.push_str(cluster);
            let mut attrs = attrs;
            attrs._pad[0] |= FLAG_CLUSTER;
            let ch = char::from_u32(i + CLUSTER_INDEX_BASE).expect("index was valid before");
            return Cell { ch, attrs };
        }
        // An index has to be a valid `char` to live in `ch`.  The pool
        // would have to hold a million clusters to reach the surrogate
        // range, and the measurement says it holds tens; past that the
        // cluster degrades to its base codepoint, which is exactly
        // today's behaviour.
        let idx = self.clusters.len();
        let Some(as_char) = u32::try_from(idx)
            .ok()
            .and_then(|i| i.checked_add(CLUSTER_INDEX_BASE))
            .and_then(char::from_u32)
        else {
            return Cell { ch: crate::grapheme::cluster_first_codepoint(cluster), attrs };
        };
        self.clusters.push(cluster.to_string());
        let mut attrs = attrs;
        attrs._pad[0] |= FLAG_CLUSTER;
        Cell { ch: as_char, attrs }
    }

    /// What a cell says, as text.
    ///
    /// One codepoint for an ordinary cell; the whole cluster for one
    /// that points into the pool.  Returns `None` for a cell whose
    /// index is stale, which a sweep can produce and which reads as
    /// the blank it will be overwritten with.
    pub fn cluster_text(&self, cell: &Cell) -> Option<&str> {
        self.cluster_text_at(cell.cluster_index()?)
    }

    /// The pool entry the cell at (`col`, `row`) points at, if any.
    #[inline]
    pub fn cluster_index_at(&self, col: u16, row: u16) -> Option<u32> {
        self.cells[self.phys_row(row) * self.cols as usize + col as usize].cluster_index()
    }

    /// The text of pool entry `idx`, for a reader holding stored cells.
    pub fn cluster_text_at(&self, idx: u32) -> Option<&str> {
        self.clusters.get(idx as usize).map(|s| &**s)
    }

    /// The clusters on the row a viewport row is showing, appended to
    /// `out`, which is cleared first.
    ///
    /// Asked once per row rather than once per cell, because that is
    /// how the answer is stored: a scrollback line's clusters are one
    /// record, and a file-backed pane fetches it with one read. A
    /// per-cell question would be one read per cell per frame.
    ///
    /// Rows still on screen have nothing here -- their cells point into
    /// the live pool and [`Self::cluster_text`] answers for them. So a
    /// reader asks this per row, then asks the pool per cell, and
    /// [`cluster_in_row`] for whatever the pool does not know.
    ///
    /// Same argument order as `cell_at_view`, and the same index
    /// arithmetic, so the two cannot disagree about which row is meant.
    pub fn row_clusters_at_view(
        &self,
        view_offset: u16,
        viewport_row: u16,
        out: &mut Vec<(u16, String)>,
    ) -> bool {
        out.clear();
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            return false;
        }
        let from_end = abs - rows;
        let sb_len = self.scrollback_len();
        if from_end >= sb_len {
            return false;
        }
        let line = sb_len - 1 - from_end;
        if self.scrollback.keeps_cluster_text() {
            return self.scrollback.clusters_at(line, out);
        }
        let Some(row) = self.sb_clusters.get(line) else {
            return false;
        };
        out.extend(row.iter().cloned());
        !out.is_empty()
    }

    /// Drop pool entries no cell points at.
    ///
    /// Sweeping, not reference counting — see the field's own comment
    /// for the measurement that decided it.  Indices move, so every
    /// cell that survives is repointed here; a cell pointing past the
    /// end afterwards is one this sweep could not see, and it reads as
    /// blank rather than as somebody else's cluster.
    pub fn sweep_clusters(&mut self) {
        if self.clusters.is_empty() {
            return;
        }
        let mut keep = vec![false; self.clusters.len()];
        for c in &self.cells {
            if let Some(i) = c.cluster_index()
                && (i as usize) < keep.len()
            {
                keep[i as usize] = true;
            }
        }
        let mut remap = vec![u32::MAX; self.clusters.len()];
        let mut next = 0u32;
        let mut kept: Vec<String> = Vec::new();
        for (i, k) in keep.iter().enumerate() {
            if *k {
                remap[i] = next;
                next += 1;
                kept.push(std::mem::take(&mut self.clusters[i]));
            }
        }
        // No early return when nothing was dropped: a cell can point
        // past the end of the pool — a torn read, a format that moved
        // — and it still has to be cleared.  Skipping the pass because
        // the pool did not shrink left exactly that cell holding a
        // flag with nothing behind it.
        for c in &mut self.cells {
            if let Some(i) = c.cluster_index() {
                match remap.get(i as usize).copied() {
                    Some(n) if n != u32::MAX => {
                        *c = c.with_cluster_char(char::from_u32(n + CLUSTER_INDEX_BASE).unwrap_or(' '));
                    }
                    _ => *c = c.with_char(' '),
                }
            }
        }
        self.clusters = kept;
    }

    /// How many clusters the pool is holding.  For the tests and for
    /// anything that wants to know the grid is not growing.
    pub fn cluster_pool_len(&self) -> usize {
        self.clusters.len()
    }

    /// Turn any cluster cell in a row back into its base codepoint.
    ///
    /// Called just before a row is handed to scrollback, because a
    /// scrollback row is stored as plain `Cell`s with nowhere to keep
    /// a pool — an index in one would point into a pool that sweeps
    /// without it, i.e. at whatever cluster later took that slot.
    /// History therefore looks the way it did before the pool existed;
    /// giving scrollback its own storage is a separate step.
    ///
    /// Free for a pane that never printed one: the pool is empty and
    /// this is a single test.  Safe to mutate in place because the row
    /// is on its way off the screen and will be overwritten by the
    /// fill behind it.
    /// Keep the cluster text of the line just pushed into scrollback.
    ///
    /// Where it goes depends on which scrollback this is, and the
    /// difference is not a preference. The in-RAM variant has nowhere
    /// but the grid, and the ring bounds the mirror. The file-backed
    /// one has a sidecar, and must use it: its history outlives the
    /// process, so a mirror could not answer for the lines written
    /// before this one started -- and bounded by `scrollback.len()`,
    /// which is the whole history, it would never be trimmed. Measured
    /// before this existed: 2000 entries for 2000 lines, growing for as
    /// long as the session runs.
    ///
    /// Must be called after the line itself is pushed: the sidecar
    /// addresses it by its index in the file.
    pub(super) fn keep_row_clusters(&mut self, row_clusters: Vec<(u16, String)>) {
        if !self.scrollback.keeps_cluster_text() {
            self.sb_clusters.push_back(row_clusters);
            return;
        }
        // Only lines that have some touch the sidecar; the test is one
        // `is_empty` on the scroll path.
        if !row_clusters.is_empty() {
            self.scrollback.cluster_last_line(&row_clusters);
        }
    }

    pub(super) fn degrade_clusters_in_row(&mut self, start: usize, cols: usize) -> Vec<(u16, String)> {
        if self.clusters.is_empty() {
            return Vec::new();
        }
        let mut kept = Vec::new();
        for i in start..start + cols {
            let Some(idx) = self.cells[i].cluster_index() else { continue };
            // Moved, not copied.  A pool entry has exactly one cell
            // pointing at it — `cluster_cell` pushes one per call and
            // the reuse path only ever rewrites the entry belonging to
            // the cell it is replacing — and that cell is the one being
            // degraded here, so nothing is left referring to it.
            // Copying instead cost 34% on a cluster-heavy stream
            // (39.6 -> 26.1 MB/s on mini), because a scrolled row full
            // of clusters is a heap allocation per cluster on the
            // scroll path.  The emptied slot is swept like any other.
            let (base, text) = match self.clusters.get_mut(idx as usize) {
                Some(c) => (
                    crate::grapheme::cluster_first_codepoint(c),
                    Some(std::mem::take(c)),
                ),
                None => (' ', None),
            };
            if let Some(t) = text {
                kept.push(((i - start) as u16, t));
            }
            self.cells[i] = self.cells[i].with_char(base);
        }
        kept
    }
}

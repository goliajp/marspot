// CARVE-OUT: pre-existing file: the cell grid and bounded scrollback; shrink on every touch
//! Terminal cell grid + bounded scrollback.
//!
//! `Grid` is a passive container: a rectangle of `Cell`s, a cursor position,
//! and a ring of scrolled-off lines.  It exposes primitive operations
//! (`set_cell`, `set_cursor`, `scroll_up`) and lets the emulator layer
//! (`terminal::Handler`) implement VT semantics on top of them.
//!
//! Scrollback is a ring buffer with a fixed capacity allocated at
//! construction.  This is **load-bearing** for the project's "cannot get
//! slower over time" commitment: terminal output is unbounded, but our
//! memory cost is not.  Once the ring is full, oldest lines are evicted
//! O(1).  Disk-backed scrollback (truly unlimited history) is a later
//! phase; it will live behind the same `Grid` interface.

mod cell;
mod numbering;
mod packed;
mod packed_style;
pub use packed::PackedCell;
mod prompt;
mod row_hash;
pub use prompt::PromptMark;
pub(crate) mod style;
mod width;
pub use cell::*;
pub use style::{PackedStyle, StyleTable};

/// A cell as the grid stores it.  Readers outside the grid's own hot
/// paths take [`Cell`] through [`Grid::cell`] and its siblings, so the
/// storage can change without them.
pub type StoredCell = PackedCell;
pub use width::{char_width, is_ambiguous_width, is_enclosed_alphanumeric};


/// Default scrollback capacity — 10 000 lines.  At 80 cols × ~12 bytes/cell
/// this is ~9 MB per terminal pre-allocated.  Tunable via
/// [`Grid::with_scrollback`].
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

use crate::scrollback::Scrollback;

pub struct Grid {
    cols: u16,
    rows: u16,
    /// Cell storage is row-major but **rotated**: logical row 0 lives at
    /// physical row `top_row`, logical row 1 at `(top_row + 1) % rows`,
    /// etc.  This turns `scroll_up(1)` from a `(rows-1) × cols` memcpy
    /// (the displaced rows shift up by one) into an O(cols) blank +
    /// pointer bump, since the new top simply becomes the cell after
    /// the old top.  Every cell access does one extra add+mod, but
    /// scrolls are by far the more frequent shape on real workloads
    /// (`cat large.log` triggers one scroll per line, hundreds of
    /// thousands of times for a 32 MB file).
    cells: Vec<StoredCell>,
    /// Grapheme clusters of more than one codepoint, pointed at by
    /// cells carrying `FLAG_CLUSTER`.
    ///
    /// Swept, not reference counted.  The measurement that decided
    /// that: 55 such clusters in forty million, across 180 MB of real
    /// pane output, with three of six panes having none — so the pool
    /// holds tens of entries and a counter per entry would be
    /// machinery for a thing that does not happen.  `sweep_clusters`
    /// drops what no cell points at; it runs when the pool has grown,
    /// not on a timer, so an idle grid does no work.
    clusters: Vec<String>,
    /// Cursor as (col, row).  Always bounded to [0, cols-1] x [0, rows-1].
    cursor_col: u16,
    cursor_row: u16,
    reach: Option<Reach>,
    /// Physical row index of logical row 0 (`0..rows`).  Bumped by
    /// `scroll_up`; reset to 0 by `resize`.
    top_row: u16,
    scrollback: Scrollback,
    /// Monotonically-increasing count of lines pushed into scrollback
    /// over this grid's lifetime.  Increments once per row inside
    /// `scroll_up`, so callers can observe "how many new lines have
    /// rolled into history since I last checked" without polling
    /// `scrollback_len` (which goes flat once the ring is full).
    /// Drives the selection-follows-content invariant in main.rs:
    /// after each pump, Marspot bumps the selection's abs coords by
    /// this delta so the highlight stays on the same content as it
    /// shifts up into scrollback.
    scroll_push_count: u64,
    /// Per-row "this row is a soft continuation of the previous row"
    /// flags — set by the emulator when DECAWM autowrap flows a
    /// logical line across rows, cleared on scroll-fill / region
    /// scrolls / explicit reset.  Indexed by **physical** row and
    /// rotated with `top_row` exactly like `cells`.  This is what
    /// lets `resize` re-wrap content instead of truncating it.
    wrapped: Vec<bool>,
    /// Continuation flags for scrollback lines, parallel to the
    /// `Scrollback` ring (front = oldest).  Lives here rather than in
    /// `Scrollback` so the disk variant's record format stays pure
    /// cells; the flags don't need to outlive the process any more
    /// than the anon-mmap ring does.  Kept in lockstep with ring
    /// eviction by trimming to `scrollback.len()` after each push.
    sb_wrapped: std::collections::VecDeque<bool>,
    /// Clusters that were on a row when it left the screen, by column.
    ///
    /// A scrollback row is plain cells with nowhere to keep a pool, so
    /// the cells themselves hold the base codepoint — which is what a
    /// binary that predates this reads, and what one that rolls back
    /// to it will read again.  The rest of each cluster lives here,
    /// keyed by position, and is applied on the way out.
    ///
    /// Parallel to `sb_wrapped` and trimmed with it.  Almost every row
    /// has none: real output measured 55 multi-codepoint clusters in
    /// forty million, so the `Vec` is empty and costs a pointer.
    sb_clusters: std::collections::VecDeque<Vec<(u16, String)>>,
    /// The URIs of the OSC 8 links cells point at; see `links`.
    links: links::LinkPool,
    /// The links of rows that left the screen, for history kept in
    /// RAM; see `links::LinkHistory`.
    sb_links: links::LinkHistory,
    /// Command boundaries for the rows on screen, by physical row.
    /// See [`PromptMark`].
    prompt: Vec<PromptMark>,
    /// The same for rows that have scrolled off.  Parallel to
    /// `sb_wrapped` and trimmed with it.
    sb_prompt: std::collections::VecDeque<PromptMark>,
    /// Consecutive all-blank rows most recently filed into scrollback.
    /// Read only by the region path — see [`MAX_FILED_BLANK_RUN`].
    filed_blank_run: u16,
    /// Per physical row: where its default cells and its default
    /// attributes start.
    marks: Vec<RowMark>,
    /// The attribute sets the cells use, numbered.
    styles: StyleTable,
    /// While `styles` sweeps: the next physical row to look at.
    sweep_row: u16,
    /// Writes note which attribute numbers each row holds, in its
    /// mark: only for history kept in RAM, which keeps a row as it is
    /// when it knows them.
    notes_styles: bool,
}

/// How many rows of chrome a TUI may reserve above its scrolling
/// content before rows leaving that content stop counting as history.
///
/// One: the shape every agent TUI measured so far uses (a title or
/// status line, then the transcript, then an input box at the bottom).
/// Raising it further would start counting a band animated in the
/// middle of a screen as history.
const MAX_RESERVED_HEADER_ROWS: u16 = 1;

/// How many consecutive all-blank rows a TUI's content area may file
/// into history before the rest are dropped.
///
/// Three, taken from the data rather than picked: across two real
/// codex sessions' scrollback the runs of blank lines are 1 row (214
/// and 480 times), 2 rows (12 and 24), and 3 rows (once) — and then
/// one run of **53**, a whole screen of nothing.  Anything up to three
/// is the transcript's own spacing; a screenful is an empty region
/// being scrolled past, which is what a pane shows as a black band
/// with history on both sides of it (2026-09-09 field report).
///
/// This applies ONLY to the region path.  `scroll_up` — a shell or a
/// full-screen program scrolling normally — still files every row: a
/// blank line there is something the user made (Enter on an empty
/// prompt, `echo ""`, a paragraph break), and dropping those was tried
/// once and reverted (F3+12.1).
const MAX_FILED_BLANK_RUN: u16 = 3;

mod cluster_mirror;
mod clusters;
mod cursor;
pub use cursor::Reach;
mod history;
mod links;
mod reflow;
pub use links::{LINK_POOL_MAX, ROW_LINK_BYTES, RowExtras, RowLink, strip_links};
mod row_mark;
mod row_styles;
pub use row_styles::{ROW_STYLES, RowStyles};
mod scroll;
pub use cluster_mirror::{UNKNOWN_CLUSTER, cluster_in_row};
pub use row_mark::RowMark;

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES)
    }

    pub fn with_scrollback(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        Self::with_scrollback_kind(
            cols,
            rows,
            Scrollback::memory(scrollback_lines, cols as usize),
        )
    }

    /// Construct with a caller-supplied scrollback (Memory or File).
    /// `Terminal::new` calls this so it can pick the File variant
    /// when `MARSPOT_SESSION_ID` is set without dragging the env
    /// check through the Grid API.
    pub fn with_scrollback_kind(cols: u16, rows: u16, scrollback: Scrollback) -> Self {
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        let cells = vec![StoredCell::default(); cols as usize * rows as usize];
        // Pre-size the flag mirror to the ring's capacity so the
        // per-scrolled-line push in `scroll_up` (per-byte-path hot
        // for `cat`) never reallocates.  1 byte per line — 10 KB at
        // the default 10 000-line scrollback.
        let sb_capacity = scrollback.capacity();
        let notes_styles = !scrollback.is_persistent();
        Self {
            cols,
            rows,
            cells,
            clusters: Vec::new(),
            cursor_col: 0,
            cursor_row: 0,
            reach: None,
            top_row: 0,
            scrollback,
            scroll_push_count: 0,
            wrapped: vec![false; rows as usize],
            prompt: vec![PromptMark::None; rows as usize],
            sb_wrapped: std::collections::VecDeque::with_capacity(sb_capacity + 1),
            sb_clusters: std::collections::VecDeque::with_capacity(sb_capacity + 1),
            links: Default::default(),
            sb_links: Default::default(),
            sb_prompt: std::collections::VecDeque::with_capacity(sb_capacity + 1),
            filed_blank_run: 0,
            marks: vec![RowMark::default(); rows as usize],
            styles: StyleTable::new(cols as usize * rows as usize),
            sweep_row: 0,
            notes_styles,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }
    /// Translate a logical row to its physical index in `cells`.
    #[inline]
    fn phys_row(&self, logical: u16) -> usize {
        let r = self.top_row as usize + logical as usize;
        if r >= self.rows as usize {
            r - self.rows as usize
        } else {
            r
        }
    }

    pub fn cell(&self, col: u16, row: u16) -> Cell {
        debug_assert!(col < self.cols && row < self.rows);
        let pr = self.phys_row(row);
        let idx = pr * self.cols as usize + col as usize;
        self.expand(self.cells[idx])
    }

    /// A stored cell as a reader sees it.
    #[inline(always)]
    pub fn expand(&self, cell: StoredCell) -> Cell {
        cell.expand(&self.styles)
    }


    /// Mark / unmark logical `row` as a soft continuation of the row
    /// above (DECAWM autowrap flowed a logical line across them).
    /// The emulator calls this from the deferred-wrap path; `resize`
    /// uses the flags to re-wrap instead of truncate.
    pub fn set_row_wrapped(&mut self, row: u16, wrapped: bool) {
        let pr = self.phys_row(row.min(self.rows - 1));
        self.wrapped[pr] = wrapped;
    }

    pub fn row_wrapped(&self, row: u16) -> bool {
        let pr = self.phys_row(row.min(self.rows - 1));
        self.wrapped[pr]
    }

    /// What a shell said about this row, if anything.  See
    /// [`PromptMark`].
    pub fn set_row_prompt(&mut self, row: u16, mark: PromptMark) {
        let pr = self.phys_row(row.min(self.rows - 1));
        self.prompt[pr] = mark;
    }

    pub fn row_prompt(&self, row: u16) -> PromptMark {
        let pr = self.phys_row(row.min(self.rows - 1));
        self.prompt[pr]
    }

    /// The mark on scrollback line `idx` (0 = oldest), the same
    /// indexing `scrollback_line` uses.
    pub fn scrollback_prompt(&self, idx: usize) -> PromptMark {
        // The same split `scrollback_wrapped` draws: the file holds
        // what the session ever wrote, the mirror only what this
        // process pushed.  An L3 that re-execs starts with an empty
        // mirror against a history thousands of lines long, and every
        // row of it would answer "no mark".
        if self.scrollback.keeps_prompt_marks() {
            return self.scrollback.prompt_at(idx);
        }
        self.sb_prompt.get(idx).copied().unwrap_or_default()
    }

    /// The view offset that puts the nearest prompt above the current
    /// one at the top of the screen, or `None` when there is no
    /// earlier prompt to go to.
    ///
    /// `view_offset` counts rows scrolled back from the live screen,
    /// the way `cell_at_view` does. Scanning is what kitty does too:
    /// the marks are a byte a row and a screenful is a few dozen, so
    /// there is nothing here worth an index.
    pub fn prompt_above(&self, view_offset: u16) -> Option<u16> {
        let sb = self.scrollback.len();
        // The absolute line index currently at the top of the view,
        // counting scrollback from 0 and then the live rows.
        let top_abs = (sb + self.rows as usize).checked_sub(
            self.rows as usize + view_offset as usize,
        )?;
        for abs in (0..top_abs).rev() {
            let mark = if abs < sb {
                self.scrollback_prompt(abs)
            } else {
                self.row_prompt((abs - sb) as u16)
            };
            if mark == PromptMark::PromptStart {
                // Put that line at the top: everything below it,
                // minus a screen, is how far back that is.
                let below = sb + self.rows as usize - abs;
                return Some((below.saturating_sub(self.rows as usize)) as u16);
            }
        }
        None
    }

    /// Continuation flag for scrollback line `idx` (same indexing as
    /// `scrollback_line`: 0 = oldest).
    ///
    /// The file-backed variant stores the flag beside the line, and
    /// that copy is the one to believe: `sb_wrapped` only holds what
    /// THIS process pushed, while the file holds everything the
    /// session ever wrote.  An L3 that re-execs itself starts a fresh
    /// mirror against a scrollback that is already thousands of lines
    /// long, so every index lands past the mirror's end and every row
    /// in the history answers "not a continuation".  Measured: the
    /// same row of the same `scrollback.bin` reads `true` from the
    /// process that wrote it and `false` from the one that reattached
    /// — after which a soft-wrapped URL in history stops merging, its
    /// first row keeps a truncated link and its second row has none
    /// (2026-09-08 field report).
    pub fn scrollback_wrapped(&self, idx: usize) -> bool {
        if self.scrollback.keeps_wrapped_flags() {
            return self.scrollback.wrapped_at(idx);
        }
        self.sb_wrapped.get(idx).copied().unwrap_or(false)
    }

    /// True when the row addressed by `(view_offset, viewport_row)` is
    /// a DECAWM soft-wrap continuation of the row physically above it.
    /// Parallels `cell_at_view`: same translation from viewport coords
    /// to either a live row or a scrollback line.  Out-of-range coords
    /// return false (no wrap signal == "treat as logical line break").
    ///
    /// Callers walking the viewport row-by-row use this to merge a
    /// soft-wrapped logical line into one unit (selection copy without
    /// the spurious `\n`, link scanning that survives the wrap, etc.).
    pub fn wrapped_at_view(&self, view_offset: u16, viewport_row: u16) -> bool {
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            return self.row_wrapped((rows - 1 - abs) as u16);
        }
        let from_end = abs - rows;
        let sb_len = self.scrollback_len();
        if from_end < sb_len {
            return self.scrollback_wrapped(sb_len - 1 - from_end);
        }
        false
    }

    /// Drop every continuation flag (live + scrollback).  Used by
    /// full-screen erase — once the screen is wiped, gluing the new
    /// content to pre-wipe history would corrupt reflow.
    ///
    /// Reaches the live rows and the in-RAM mirror only. A file-backed
    /// pane keeps each flag in the record beside its line, and those
    /// are not rewritten, so for one of those this clears nothing that
    /// a later reflow will read. Pre-existing; clearing them means
    /// rewriting every record, which full-screen erase has no business
    /// doing on the parse thread.
    pub fn clear_all_wrapped(&mut self) {
        self.wrapped.iter_mut().for_each(|w| *w = false);
        for w in self.sb_wrapped.iter_mut() {
            *w = false;
        }
    }

    /// Return the cell at the given viewport position, honouring
    /// `view_offset` (lines scrolled up from live, 0 = live view).
    /// Pulls from the live grid for visible rows and from
    /// `scrollback.cell_at` for scrolled-up rows; returns a default
    /// cell past the oldest scrollback line.  Both renderers
    /// (`render.rs` AppKit, `render_metal.rs` Metal) call through
    /// here so they read the same view of the grid.
    pub fn cell_at_view(&self, view_offset: u16, col: u16, viewport_row: u16) -> Cell {
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            return self.cell(col, (rows - 1 - abs) as u16);
        }
        let from_end = abs - rows;
        let sb_len = self.scrollback_len();
        if from_end < sb_len {
            let sb_idx = sb_len - 1 - from_end;
            if let Some(c) = self.scrollback_cell(sb_idx, col) {
                return c;
            }
        }
        Cell::default()
    }

    /// The cells of a viewport row, when the row is one of the live
    /// grid's: one slice, for a reader that takes the whole row.  `None`
    /// for a row from the history, which [`Self::cell_at_view`] reads a
    /// cell at a time.
    pub fn live_row_at_view(&self, view_offset: u16, viewport_row: u16) -> Option<&[StoredCell]> {
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs >= rows {
            return None;
        }
        let start = self.phys_row((rows - 1 - abs) as u16) * self.cols as usize;
        Some(&self.cells[start..start + self.cols as usize])
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }
    /// F3+10 — flush File-variant BufWriter tail to kernel page
    /// cache before execv.  Memory / Disk no-op.  See
    /// `Scrollback::flush_for_handoff` for the rationale.
    pub fn scrollback_flush_for_handoff(&self) {
        self.scrollback.flush_for_handoff();
    }
    pub fn scrollback_capacity(&self) -> usize {
        self.scrollback.capacity()
    }
    /// B3 — hand back an off-thread search snapshot of the File-backed
    /// scrollback (returns None for Memory/Disk variants).  Used by
    /// the L3 main loop on `SearchScrollback` to feed an isolated
    /// view to the search worker thread.
    pub fn file_scrollback_snapshot(&self) -> Option<crate::scrollback::FileSnapshot> {
        self.scrollback.file_snapshot()
    }

    /// B4 — snapshot the currently-visible grid rows (oldest visible
    /// first, newest visible last) for off-thread search.  Each entry
    /// is `(row_cells, row_wrapped)` matching the same shape the
    /// scrollback-search engine consumes via `SearchSource::line` /
    /// `wrapped`.  Cells are owned (cloned) so the snapshot is fully
    /// independent of the live grid — the worker thread can iterate
    /// without locking and without racing the PTY pump.
    ///
    /// The cost is O(rows × cols) cell clones, paid once per
    /// `SearchRequest` event on the main loop.  Bounded by the live
    /// grid (typically ~50 × 200 = 10 k cells) — well inside the
    /// "single SearchRequest is a cold event" budget.
    pub fn live_grid_snapshot_for_search(&self) -> Vec<(Vec<Cell>, bool)> {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows as u16 {
            let pr = self.phys_row(r);
            let row_cells = self.cells[pr * cols..(pr + 1) * cols].iter().map(|&c| self.expand_cold(c)).collect();
            let wrapped = self.wrapped[pr];
            out.push((row_cells, wrapped));
        }
        out
    }
    /// One cell from scrollback by `(line_idx, col)`.  Hot path —
    /// avoids per-line allocation that the disk-backed variant
    /// would otherwise need to materialise a slice.
    pub fn scrollback_cell(&self, line_idx: usize, col: u16) -> Option<Cell> {
        self.scrollback.cell_at(line_idx, col as usize)
    }
    /// One whole scrollback line, owned.  For tests + the headless
    /// `--snapshot` path; production rendering uses `scrollback_cell`.
    pub fn scrollback_line(&self, idx: usize) -> Option<Vec<Cell>> {
        self.scrollback.line_to_vec(idx)
    }
    /// Resize the visible grid **without losing content**.
    ///
    /// Rows-only change: cells and scrollback are preserved verbatim.
    /// Growing adds blank rows at the bottom; shrinking first drops
    /// blank rows from the bottom (below the cursor), then pushes
    /// rows from the top into scrollback — same as every mainstream
    /// terminal.
    ///
    /// Column change: full reflow.  Scrollback + live rows are
    /// gathered into logical lines using the autowrap continuation
    /// flags, re-wrapped at the new width (wide CJK/emoji pairs are
    /// never split), and redistributed across scrollback + the live
    /// grid.  The cursor follows its logical position.  This is what
    /// keeps a shrink → grow round trip lossless instead of leaving
    /// every line truncated at the narrowest width it ever saw.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        if cols == self.cols {
            self.resize_rows_only(rows);
            return;
        }
        self.reflow(cols, rows);
    }

    /// [`Self::expand`] kept out of line, for the resize code: it reads
    /// every cell once, and a copy of the expansion at each of its
    /// sites cost kilobytes of binary for no speed anyone sees.
    #[inline(never)]
    fn expand_cold(&self, cell: StoredCell) -> Cell {
        self.expand(cell)
    }

    fn cell_cold(&self, col: u16, row: u16) -> Cell {
        self.expand_cold(self.cells[self.phys_row(row) * self.cols as usize + col as usize])
    }

    /// Height-only resize: lossless by construction.
    fn resize_rows_only(&mut self, rows: u16) {
        let cols = self.cols as usize;
        if rows > self.rows {
            // Grow: append blank rows at the bottom.  De-rotate into a
            // fresh buffer so indexing stays simple.
            let mut new_cells = vec![Cell::default(); cols * rows as usize];
            let mut new_wrapped = vec![false; rows as usize];
            let mut new_prompt = vec![PromptMark::None; rows as usize];
            for r in 0..self.rows {
                let dst = r as usize * cols;
                for c in 0..self.cols {
                    new_cells[dst + c as usize] = self.cell_cold(c, r);
                }
                new_wrapped[r as usize] = self.wrapped[self.phys_row(r)];
                new_prompt[r as usize] = self.prompt[self.phys_row(r)];
            }
            self.replace_cells(self.cols, rows, &new_cells);
            self.wrapped = new_wrapped;
            self.prompt = new_prompt;
            self.rows = rows;
            self.top_row = 0;
            self.forget_row_marks();
            return;
        }
        // Shrink: prefer dropping blank rows below the cursor; push
        // the remainder from the top into scrollback so nothing is
        // lost and the cursor stays on screen.
        let mut need = (self.rows - rows) as usize;
        let mut last_keep = self.rows - 1; // drop blank rows from the bottom
        while need > 0 && last_keep > self.cursor_row && self.row_is_blank(last_keep) {
            last_keep -= 1;
            need -= 1;
        }
        if need > 0 {
            let push = need as u16;
            self.scroll_up(push, Cell::default());
            self.cursor_row = self.cursor_row.saturating_sub(push);
        }
        let mut new_cells = vec![Cell::default(); cols * rows as usize];
        let mut new_wrapped = vec![false; rows as usize];
        let mut new_prompt = vec![PromptMark::None; rows as usize];
        for r in 0..rows {
            let dst = r as usize * cols;
            for c in 0..self.cols {
                new_cells[dst + c as usize] = self.cell_cold(c, r);
            }
            new_wrapped[r as usize] = self.wrapped[self.phys_row(r)];
            new_prompt[r as usize] = self.prompt[self.phys_row(r)];
        }
        self.replace_cells(self.cols, rows, &new_cells);
        self.wrapped = new_wrapped;
        self.prompt = new_prompt;
        self.rows = rows;
        self.top_row = 0;
        self.forget_row_marks();
        self.cursor_row = self.cursor_row.min(rows - 1);
    }

    fn row_is_blank(&self, row: u16) -> bool {
        let start = self.phys_row(row) * self.cols as usize;
        self.cells[start..start + self.cols as usize]
            .iter()
            .all(|c| *c == PackedCell::BLANK)
    }
}

#[cfg(test)]
mod region_scrollback_tests {
    use super::*;

    /// A TUI scrolling an empty content area must not fill history with
    /// a screenful of nothing.
    ///
    /// codex does exactly that while it starts: its region scrolls
    /// before there is anything in it, one blank row at a time, and the
    /// pane ends up with a black band between the previous session's
    /// output and the new one.  Measured on a real session: a run of 53
    /// blank rows in a 63-row pane.
    #[test]
    fn a_region_scrolling_an_empty_area_stops_filing_blanks() {
        let mut g = Grid::new(20, 8);
        // 30 scrolls of an empty region — far more than a screenful.
        for _ in 0..30 {
            g.scroll_up_region(0, 5, 1, Cell::default());
        }
        assert_eq!(
            g.scrollback_len(),
            MAX_FILED_BLANK_RUN as usize,
            "only the run that reads as spacing is kept",
        );
    }

    /// ...and the run resets, so spacing between two blocks survives.
    #[test]
    fn content_between_blank_runs_resets_the_allowance() {
        let mut g = Grid::new(20, 8);
        for _ in 0..10 {
            g.scroll_up_region(0, 5, 1, Cell::default());
        }
        let after_blanks = g.scrollback_len();
        g.set_cell(0, 0, Cell { ch: 'x', ..Cell::default() });
        g.scroll_up_region(0, 5, 1, Cell::default());
        for _ in 0..10 {
            g.scroll_up_region(0, 5, 1, Cell::default());
        }
        assert_eq!(
            g.scrollback_len(),
            after_blanks + 1 + MAX_FILED_BLANK_RUN as usize,
            "the content row, then a fresh allowance of spacing",
        );
    }

    /// The shell path is untouched: a blank line there is something the
    /// user made, and dropping those was tried once and reverted
    /// (F3+12.1).
    #[test]
    fn a_full_screen_scroll_still_files_every_blank_line() {
        let mut g = Grid::new(20, 8);
        for _ in 0..30 {
            g.scroll_up(1, Cell::default());
        }
        assert_eq!(g.scrollback_len(), 30, "every blank line is history here");
    }

    /// A TUI that reserves a header row still scrolls its history out
    /// of the screen.
    ///
    /// claudecode drives `CSI 2;57 r` + `CSI <n> S` — the same shape as
    /// codex's `CSI 1;56 r`, one row lower because it keeps a title
    /// line.  Under a strict `top == 0` its transcript went nowhere,
    /// which is why one agent pane could be scrolled and selected
    /// across and the other could not.
    #[test]
    fn a_region_one_row_below_the_top_still_files_history() {
        let mut g = Grid::new(20, 8);
        for r in 0..8u16 {
            for (i, ch) in format!("row{r}").chars().enumerate() {
                g.set_cell(i as u16, r, Cell { ch, ..Cell::default() });
            }
        }
        // A header at row 0, content in 1..=5, chrome below.
        g.scroll_up_region(1, 5, 2, Cell::default());
        assert_eq!(g.scrollback_len(), 2, "the rows that left the band are history");
        let filed: Vec<String> = (0..2)
            .map(|i| {
                g.scrollback_line(i)
                    .map(|cs| cs.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(filed, ["row1", "row2"]);
    }

    /// ...but a band further down is a window-internal shuffle, and its
    /// rows are not history.  This is the line the tolerance stops at.
    #[test]
    fn a_band_two_rows_below_the_top_files_nothing() {
        let mut g = Grid::new(20, 8);
        g.scroll_up_region(2, 5, 2, Cell::default());
        assert_eq!(g.scrollback_len(), 0);
    }

    /// A soft-wrap flag has to survive the scroll that files the row.
    ///
    /// The band's flags were wiped before the push, so every line a
    /// top-anchored region scrolled into history was recorded as "not
    /// a continuation" — all of a codex pane's history, measured at
    /// 1024 lines and zero flags on a live `scrollback.bin`.
    #[test]
    fn a_region_scroll_files_the_continuation_flag_it_was_given() {
        let mut g = Grid::new(20, 6);
        // Content in every row: a blank one would now be collapsed by
        // `MAX_FILED_BLANK_RUN`, and this test is about the flag.
        for r in 0..6u16 {
            g.set_cell(0, r, Cell { ch: char::from(b'a' + r as u8), ..Cell::default() });
        }
        // Row 1 continues row 0; row 3 continues row 2.
        g.set_row_wrapped(1, true);
        g.set_row_wrapped(3, true);
        // codex's shape: a region anchored at row 0 with the bottom
        // rows reserved.  Scroll four rows out of it.
        g.scroll_up_region(0, 4, 4, Cell::default());
        assert_eq!(g.scrollback_len(), 4);
        let flags: Vec<bool> = (0..4).map(|i| g.scrollback_wrapped(i)).collect();
        assert_eq!(
            flags,
            [false, true, false, true],
            "each filed line keeps the flag its row carried",
        );
    }

    /// The row that arrives at the top of the band keeps its flag only
    /// when the row it continued went to scrollback.  In a band below
    /// row 0 that predecessor is discarded, so the flag would claim a
    /// continuation of whatever unrelated row sits above the region.
    #[test]
    fn a_band_below_row_zero_drops_the_flag_at_its_top() {
        let mut g = Grid::new(20, 6);
        g.set_row_wrapped(3, true);
        g.scroll_up_region(2, 5, 1, Cell::default());
        assert_eq!(g.scrollback_len(), 0, "a band below row 0 files nothing");
        assert!(
            !g.row_wrapped(2),
            "the row that moved up to the band's top continued a row that is gone",
        );

        // ...but the same shift one row further down keeps it: its
        // predecessor is still inside the band, one row above.
        let mut g = Grid::new(20, 6);
        g.set_row_wrapped(4, true);
        g.scroll_up_region(2, 5, 1, Cell::default());
        assert!(g.row_wrapped(3), "the flag travelled with its row");
    }

    /// The row blanked at the bottom of the band is new content, not a
    /// continuation of anything.
    #[test]
    fn the_blanked_bottom_row_is_not_a_continuation() {
        let mut g = Grid::new(20, 6);
        g.set_row_wrapped(5, true);
        g.scroll_up_region(0, 5, 1, Cell::default());
        assert!(!g.row_wrapped(5), "the blank row carries no flag");
    }

    /// The shape a TUI uses to reserve its input box: a region
    /// anchored at row 0 with rows held back at the bottom.
    fn scroll_through_region(rows: u16, reserved: u16, n: u32) -> Grid {
        let mut g = Grid::new(20, rows);
        let bot = rows - reserved - 1;
        for i in 1..=n {
            // Write at the region's last row, then scroll the region.
            g.set_cursor(0, bot);
            for (c, ch) in format!("LINE-{i:03}").chars().enumerate() {
                g.set_cell(c as u16, bot, Cell { ch, ..Default::default() });
            }
            g.scroll_up_region(0, bot, 1, Cell::default());
        }
        g
    }

    #[test]
    fn a_region_anchored_at_the_top_feeds_scrollback() {
        // Measured in iTerm2 with the identical sequence: all 120
        // lines stay reachable.  Dropping them is what left codex
        // panes with no history at all.
        let g = scroll_through_region(40, 8, 120);
        let sb: Vec<String> = (0..g.scrollback_len())
            .filter_map(|i| g.scrollback_line(i))
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect();
        assert!(
            sb.iter().any(|l| l.starts_with("LINE-001")),
            "the oldest line must still be reachable"
        );
        // Not one of the 120 may be lost: each is either in history or
        // still on screen.  (The raw push count is no longer 120 — the
        // region also scrolled its empty top rows before any content
        // reached them, and those collapse; see `MAX_FILED_BLANK_RUN`.)
        let screen: Vec<String> = (0..40u16)
            .map(|r| (0..20u16).map(|c| g.cell(c, r).ch).collect::<String>())
            .collect();
        for i in 1..=120 {
            let want = format!("LINE-{i:03}");
            assert!(
                sb.iter().chain(screen.iter()).any(|l| l.starts_with(&want)),
                "{want} is neither in history nor on screen",
            );
        }
        // The newest line is still ON SCREEN, not in scrollback —
        // scrollback is where things go when they leave.
        let on_screen: Vec<String> = (0..40u16)
            .map(|r| (0..20u16).map(|c| g.cell(c, r).ch).collect::<String>())
            .collect();
        assert!(on_screen.iter().any(|l| l.starts_with("LINE-120")));
        assert!(
            !sb.iter().any(|l| l.starts_with("LINE-120")),
            "a line still on screen must not also be in history"
        );
    }

    #[test]
    fn a_region_below_a_header_does_not() {
        // A band inside the screen — a pager's body under a title.
        // Its rows go nowhere, exactly as before.
        let mut g = Grid::new(20, 40);
        for _ in 0..50 {
            g.scroll_up_region(3, 30, 1, Cell::default());
        }
        assert_eq!(g.scroll_push_count(), 0);
        assert_eq!(g.scrollback_len(), 0);
    }
}

#[cfg(test)]
mod tests {

    /// A soft-wrap flag has to survive the process that wrote it.
    ///
    /// `sb_wrapped` only holds what THIS process pushed; the file
    /// holds everything the session ever wrote.  An L3 that re-execs
    /// itself — which every silent update does — starts a fresh mirror
    /// against a scrollback already thousands of lines long, so every
    /// index lands past the mirror's end and all of history answers
    /// "not a continuation".  The visible cost was a URL that wrapped
    /// in history: its first row kept a truncated link and its second
    /// row had none.
    /// A row that scrolls off into a file is blanked only as far as it
    /// was written; whatever it held, and whatever the fill, the row
    /// that comes back at the bottom is the fill from edge to edge.
    #[test]
    fn a_row_scrolled_into_a_file_comes_back_blank_from_edge_to_edge() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-grid-blank-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let (cols, rows) = (8u16, 3u16);
        let sb = crate::scrollback::Scrollback::file(
            dir.join("scrollback.bin"),
            dir.join("scrollback.idx"),
            cols as usize,
            8,
        )
        .expect("open");
        let mut g = Grid::with_scrollback_kind(cols, rows, sb);
        let coloured = Cell {
            ch: ' ',
            attrs: CellAttrs { bg: Color::indexed(4), ..CellAttrs::default() },
        };
        // which columns each row has something in, before it scrolls off
        let rows_written: [&[u16]; 6] = [&[], &[0], &[0, 1, 2], &[7], &[3], &[0, 1, 2, 3, 4, 5, 6, 7]];
        for (round, fill) in [Cell::default(), coloured, Cell::default()].into_iter().enumerate() {
            for written in rows_written {
                for &col in written {
                    g.set_cell(col, 0, Cell::from('x'));
                }
                // a blank that is not the default one counts as written
                if written.len() == 1 {
                    g.set_cell(5, 0, coloured);
                }
                g.scroll_up(1, fill);
                for col in 0..cols {
                    assert_eq!(g.cell(col, rows - 1), fill, "round {round}, row {written:?}, col {col}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrapped_flag_in_the_file_survives_the_process_that_wrote_it() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-grid-wrapped-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let bin = dir.join("scrollback.bin");
        let idx = dir.join("scrollback.idx");
        let (cols, rows) = (8u16, 3u16);

        let pushed = {
            let sb = crate::scrollback::Scrollback::file(bin.clone(), idx.clone(), cols as usize, 8)
                .expect("open");
            let mut g = Grid::with_scrollback_kind(cols, rows, sb);
            // Row 1 is a continuation of row 0; scroll both out.
            g.set_row_wrapped(1, true);
            g.scroll_up(3, Cell::default());
            assert!(g.scrollback_wrapped(1), "the writer sees its own flag");
            g.scrollback_len()
        };

        let sb = crate::scrollback::Scrollback::file(bin, idx, cols as usize, 8).expect("reopen");
        let g = Grid::with_scrollback_kind(cols, rows, sb);
        assert_eq!(g.scrollback_len(), pushed, "the file kept the lines");
        assert!(
            g.scrollback_wrapped(1),
            "a process that reattached to the same scrollback must read the same flag",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn empty_grid_is_all_default_cells_with_origin_cursor() {
        let g = Grid::new(80, 24);
        assert_eq!(g.cols(), 80);
        assert_eq!(g.rows(), 24);
        assert_eq!(g.cursor(), (0, 0));
        for r in 0..24 {
            for c in 0..80 {
                assert_eq!(g.cell(c, r), Cell::default());
            }
        }
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn set_cursor_clamps_to_grid_bounds() {
        let mut g = Grid::new(10, 5);
        g.set_cursor(99, 99);
        assert_eq!(g.cursor(), (9, 4));
    }

    #[test]
    fn scroll_up_one_pushes_top_row_to_scrollback() {
        let mut g = Grid::new(3, 2);
        g.set_cell(0, 0, 'a'.into());
        g.set_cell(1, 0, 'b'.into());
        g.set_cell(2, 0, 'c'.into());
        g.set_cell(0, 1, 'd'.into());
        g.set_cell(1, 1, 'e'.into());
        g.set_cell(2, 1, 'f'.into());

        g.scroll_up(1, Cell::default());

        // Row 0 became scrollback line 0, row 1 moved up to row 0, row 1 blank.
        let sb = g.scrollback_line(0).expect("scrollback[0] should exist");
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        assert_eq!(g.cell(0, 0).ch, 'd');
        assert_eq!(g.cell(2, 0).ch, 'f');
        assert_eq!(g.cell(0, 1), Cell::default());
        assert_eq!(g.scrollback_len(), 1);
    }

    #[test]
    fn scroll_up_uses_fill_cell_for_blank_bottom() {
        // BCE: caller passes a stamped Cell; the new bottom rows must adopt it.
        let mut g = Grid::new(3, 3);
        let blank = Cell {
            ch: ' ',
            attrs: CellAttrs {
                bg: Color::indexed(1),
                ..CellAttrs::default()
            },
        };
        g.scroll_up(2, blank);
        for c in 0..3 {
            for r in 1..3 {
                assert_eq!(g.cell(c, r), blank);
            }
        }
    }

    #[test]
    fn scroll_up_more_than_height_clears_screen_and_pushes_all() {
        let mut g = Grid::new(2, 2);
        g.set_cell(0, 0, 'A'.into());
        g.set_cell(0, 1, 'B'.into());
        g.scroll_up(5, Cell::default()); // scroll farther than height
        // All 2 rows pushed (capped at rows).
        assert_eq!(g.scrollback_len(), 2);
        for r in 0..2 {
            for c in 0..2 {
                assert_eq!(g.cell(c, r), Cell::default());
            }
        }
    }

    #[test]
    fn scrollback_evicts_oldest_at_capacity() {
        let mut g = Grid::with_scrollback(2, 2, 3); // capacity 3 lines
        // Fill row 0 with 'X', row 1 blank, scroll once → scrollback[0] = "XX".
        // Repeat with markers to verify eviction order.
        for marker in ['1', '2', '3', '4'].iter() {
            g.set_cell(0, 0, Cell::from(*marker));
            g.set_cell(1, 0, Cell::from(*marker));
            g.scroll_up(1, Cell::default());
        }
        // After 4 pushes into a capacity-3 ring: '1' was evicted, ring holds
        // ['2','3','4'] in chronological order.
        assert_eq!(g.scrollback_len(), 3);
        let to_string = |line: &[Cell]| line.iter().map(|c| c.ch).collect::<String>();
        assert_eq!(to_string(&g.scrollback_line(0).unwrap()), "22");
        assert_eq!(to_string(&g.scrollback_line(1).unwrap()), "33");
        assert_eq!(to_string(&g.scrollback_line(2).unwrap()), "44");
        assert_eq!(g.scrollback_line(3), None);
    }

    #[test]
    fn clear_scrollback_zeroes_len_in_o1() {
        let mut g = Grid::with_scrollback(2, 2, 5);
        for _ in 0..5 {
            g.scroll_up(1, Cell::default());
        }
        assert_eq!(g.scrollback_len(), 5);
        g.clear_scrollback();
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.scrollback_line(0), None);
    }

    #[test]
    fn scroll_up_zero_is_noop() {
        let mut g = Grid::new(3, 2);
        g.set_cell(0, 0, 'A'.into());
        g.scroll_up(0, Cell::default());
        assert_eq!(g.cell(0, 0).ch, 'A');
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn zero_capacity_scrollback_silently_drops_pushes() {
        let mut g = Grid::with_scrollback(3, 2, 0);
        g.set_cell(0, 0, 'A'.into());
        g.scroll_up(1, Cell::default());
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.scrollback_capacity(), 0);
    }
}


#[cfg(test)]
mod mirror_growth_tests {
    use super::*;

    /// The three deques beside the scrollback ring are mirrors of what
    /// THIS process pushed.  For the in-RAM variant they are bounded by
    /// the ring.  For the file-backed one the trim bound is
    /// `scrollback.len()`, which is the whole history -- so they grow
    /// for as long as the session runs.
    ///
    /// None of the three is the right place for a file-backed pane,
    /// and for the same reason in each case: a mirror cannot answer
    /// for the lines written before this process started.
    /// `scrollback_wrapped` and `scrollback_prompt` already preferred
    /// the file's copy, so keeping them was paying per line forever for
    /// an answer nobody asks.  `sb_clusters` *was* read -- and measured
    /// at 2000 entries for 2000 lines of history, each one a `Vec` and
    /// the ones with clusters holding their text -- so it was not
    /// dropped but moved: the file's sidecar answers it now.
    #[test]
    fn the_mirrors_do_not_grow_with_the_history_of_a_file_backed_pane() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-mirror-growth-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sb = Scrollback::file(
            dir.join("scrollback.bin"),
            dir.join("scrollback.idx"),
            4,
            16,
        )
        .unwrap();
        let mut g = Grid::with_scrollback_kind(4, 2, sb);
        for _ in 0..2000 {
            g.scroll_up(1, Cell::default());
        }
        let history = g.scrollback_len();
        assert!(history > 1000, "the fixture has to build history, got {history}");
        assert!(
            g.sb_wrapped.len() <= 64,
            "wrapped mirror holds {} entries for {history} lines of history",
            g.sb_wrapped.len()
        );
        assert!(
            g.sb_prompt.len() <= 64,
            "mark mirror holds {} entries for {history} lines of history",
            g.sb_prompt.len()
        );
        assert!(
            g.sb_clusters.len() <= 64,
            "cluster mirror holds {} entries for {history} lines of history",
            g.sb_clusters.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

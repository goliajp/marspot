//! Putting glyphs into the grid: ending a cluster, writing a glyph at
//! the cursor, the CSI prologue, scrolling the region, and the DECAWM
//! deferred wrap.

use super::Handler;
use super::erase::blank_with;
use super::trace::trace_seq;
use crate::grid::Cell;
use crate::lx_debug_sampled;

impl<'a> Handler<'a> {
    /// Commit the buffered grapheme cluster to the grid: compute its
    /// width over the WHOLE buffered string (so VS16, ZWJ glue, RI
    /// pairs, and combining marks all factor in), write the cluster's
    /// base codepoint to the cell at the cursor with that width, and
    /// clear the buffer.  Does NOT touch the segmenter state — call
    /// after a `step` returned `true` and you've already seeded the
    /// state with the next cluster's first codepoint.
    ///
    /// Phase 1 limitation: only the base codepoint is committed to
    /// the cell.  Full-cluster glyph rendering (compound emoji,
    /// combining marks) lands when we move cluster storage into a
    /// Grid-side pool (task #5 follow-up).
    /// End the cluster being built.
    ///
    /// Nothing is written here any more: a glyph is committed the
    /// moment its width is known, and this only says that whatever
    /// arrives next cannot join it.  The name is kept because every
    /// caller means exactly that — a cursor move, an erase, a control
    /// code, the end of a batch.
    pub(super) fn flush_cluster_keep_cursor(&mut self) {
        self.cluster_buf.clear();
        *self.cluster_fast = None;
        *self.cluster_anchor = None;
    }

    /// Same as [`flush_cluster_keep_cursor`] but also resets the
    /// segmenter — the next codepoint will be treated as a fresh
    /// cluster start.  Use at every non-print event (control byte,
    /// escape sequence, end-of-feed) so a cursor move or CSI doesn't
    /// fuse two visually distinct clusters across the operation.
    pub(super) fn flush_cluster_for_break(&mut self) {
        self.flush_cluster_keep_cursor();
        self.grapheme_cursor.reset();
        // Empty buffer + freshly-reset cursor ARE in sync — clears
        // any fast-path debt so the next slow-path print doesn't
        // replay a stale flag.
        *self.seg_synced = true;
    }

    /// Scroll-or-step to column 0 of the next row and mark it as a
    /// soft wrap continuation.  Shared tail of the batch lanes' and
    /// `write_glyph`'s wrap paths.
    pub(super) fn wrap_to_next_row(&mut self, row: u16, rows: u16) {
        let bot = *self.scroll_bot;
        if row == bot {
            self.region_scroll_up(1);
            self.grid.set_cursor(0, row);
        } else if row + 1 < rows {
            self.grid.set_cursor(0, row + 1);
        } else {
            self.grid.set_cursor(0, rows - 1);
        }
        let (_, r) = self.grid.cursor();
        self.grid.set_row_wrapped(r, true);
    }

    /// Re-seed the segmenter after the ASCII fast path skipped it.
    /// The fast path maintains the invariant that an unsynced buffer
    /// holds at most one (ASCII) codepoint, so the replay is O(1).
    pub(super) fn resync_segmenter(&mut self) {
        if !*self.seg_synced {
            self.grapheme_cursor.reset();
            for c in self.cluster_buf.chars() {
                self.grapheme_cursor.step(c);
            }
            *self.seg_synced = true;
        }
    }

    /// Commit one already-segmented glyph (codepoint + cell width) to
    /// the grid at the cursor, handling the DECAWM deferred-wrap and
    /// wide-char wrap edge cases.  Body extracted from the old
    /// per-codepoint `print` so the cluster flush path and any future
    /// non-parser writer can share the same cursor-advance logic.
    /// Commit one glyph and return the cell it landed in, so the
    /// caller can anchor to it — a codepoint that arrives later and
    /// belongs to the same cluster amends that cell rather than
    /// starting a new one.
    pub(super) fn write_glyph(&mut self, ch: char, w: u8) -> (u16, u16) {
        // DECAWM deferred wrap: the previous glyph landed in the last
        // column and set `pending_wrap`. The wrap was deliberately
        // deferred so that a trailing `\r\n` (or any cursor move)
        // wouldn't compound with the wrap into a two-row advance —
        // the classic "every row has a blank row after it" symptom
        // when TUIs draw box borders flush against the right edge.
        self.take_pending_wrap();

        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let (mut col, mut row) = self.grid.cursor();

        // A wide glyph at the last column can't fit. Wrap first, then
        // write at the start of the new row; with DECAWM off it takes
        // the last two columns instead.
        if w == 2 && col + 1 >= cols && !self.modes.autowrap {
            col = cols.saturating_sub(2);
        } else if w == 2 && col + 1 >= cols {
            // The abandoned last column gets a NUL pad sentinel (when
            // it isn't carrying real content) so resize reflow knows
            // it's wide-wrap padding, not a space the user typed.
            if self.grid.cell(cols - 1, row) == Cell::default() {
                self.grid.set_char(cols - 1, row, '\0', self.attrs);
            }
            let bot = *self.scroll_bot;
            if row == bot {
                self.region_scroll_up(1);
                self.grid.set_cursor(0, row);
            } else if row + 1 < rows {
                self.grid.set_cursor(0, row + 1);
            } else {
                self.grid.set_cursor(0, rows - 1);
            }
            let next = self.grid.cursor();
            col = next.0;
            row = next.1;
            self.grid.set_row_wrapped(row, true);
        }

        // IRM: the glyph makes room for itself rather than taking
        // someone's place.  Everything from the cursor rightward moves
        // over by the glyph's width and whatever runs off the end of
        // the line is gone -- `vttest`'s insert page builds its line
        // that way, and so does any editor inserting mid-line without
        // repainting the rest.
        if *self.insert_mode {
            self.shift_line_right(col, row, w as u16);
        }

        // Lead cell carries the printable char.  For wide glyphs, the
        // trail cell stores NUL with the same attrs — the renderer
        // skips drawing its glyph (NUL is treated as blank), and the
        // lead glyph extends visually across both cells via its natural
        // advance width.
        let linked = self.attrs.with_link_id(self.link);
        let pen = if self.link == 0 { &*self.attrs } else { &linked };
        self.grid.set_char(col, row, ch, pen);
        if w == 2 {
            self.grid.set_char(col + 1, row, '\0', pen);
        }

        let next_col = col + w as u16;
        if next_col < cols {
            self.grid.set_cursor(next_col, row);
        } else {
            // Hit the right edge — defer the wrap. Cursor visually
            // stays at the last column; the next write will consume
            // the flag and wrap, any non-print op clears it without
            // advancing.
            self.grid.set_cursor(cols - 1, row);
            *self.pending_wrap = self.modes.autowrap;
        }
        (col, row)
    }

    /// What every control sequence does before it is acted on: ends
    /// the text run and the open cluster, and leaves its trace.
    #[inline(always)]
    pub(super) fn csi_prologue(&mut self, params: &[u16], intermediates: &[u8], byte: u8) {
        self.flush_u_buf();
        self.flush_cluster_for_break();
        trace_seq("CSI", intermediates, params, byte);
        // Every CSI through the parser lands a sampled DEBUG line.
        // 1/64 keeps cost flat under heavy apps (tmux paints ~hundreds
        // of CSI/s), but bursts above that get truncated logarithmically
        // — good enough to spot patterns ("this app drives ~50 CUP/s")
        // without flooding when an SGR-heavy app like btop runs.  Set
        // `MARSPOT_LOG_TERM=debug` to enable; default INFO is 0 ns.
        lx_debug_sampled!(
            "term.csi",
            64,
            "CSI dispatch",
            final_byte = byte as char,
            intermediates_len = intermediates.len(),
            param0 = params.first().copied().unwrap_or(0)
        );
    }

    /// Scroll the grid up by 1 line, honouring DECSTBM. When the
    /// scroll region covers the whole grid (the default) this drops
    /// to `grid.scroll_up` which also pushes to scrollback —
    /// region-bounded scrolls don't push to scrollback (they're
    /// in-grid only, e.g. TUI footer redraws).
    pub(super) fn region_scroll_up(&mut self, lines: u16) {
        let top = *self.scroll_top;
        let bot = *self.scroll_bot;
        let rows = self.grid.rows();
        if top == 0 && bot + 1 >= rows {
            self.grid.scroll_up(lines, blank_with(*self.attrs));
        } else {
            self.grid
                .scroll_up_region(top, bot, lines, blank_with(*self.attrs));
        }
    }

    /// IND — cursor down one row, scrolling the region up when it is
    /// already at the bottom margin.  `LF` is this with no carriage
    /// return, and `NEL` is this with one, so all three share it.
    pub(super) fn index(&mut self) {
        let (col, row) = self.grid.cursor();
        let rows = self.grid.rows();
        let bot = *self.scroll_bot;
        if row == bot {
            // At scroll-region bottom — scroll within region.
            self.region_scroll_up(1);
            // Cursor stays at the now-blank last row.
        } else if row + 1 < rows {
            self.grid.set_cursor(col, row + 1);
        }
        // If row+1 == rows but row != bot (cursor outside region),
        // xterm-style: cursor caps, no scroll.
    }

    /// RI — cursor UP one row, scrolling the region DOWN when it is
    /// already at the top margin.  The mirror of [`Self::index`].
    pub(super) fn reverse_index(&mut self) {
        let (col, row) = self.grid.cursor();
        if row == *self.scroll_top {
            self.region_scroll_down(1);
        } else if row > 0 {
            self.grid.set_cursor(col, row - 1);
        }
    }

    pub(super) fn region_scroll_down(&mut self, lines: u16) {
        let top = *self.scroll_top;
        let bot = *self.scroll_bot;
        self.grid
            .scroll_down_region(top, bot, lines, blank_with(*self.attrs));
    }

    /// Consume the DECAWM "deferred wrap" flag (set by print at the
    /// last column). When set, advance the cursor to the start of the
    /// next row, scrolling within the region if at the bottom — same
    /// path the immediate-wrap branch used to take. Called at the top
    /// of `print` before drawing the next glyph; cleared without
    /// advancing by any non-print operation.
    /// The check runs once per printable character; the work runs once
    /// per row.  Keeping them in one function made the whole thing a
    /// call on the hot path — it showed up as its own 5.1 % symbol in
    /// a leaf profile, which is what a function that should have been
    /// a branch looks like.  The rare half is pushed out of line.
    #[inline(always)]
    fn take_pending_wrap(&mut self) {
        if *self.pending_wrap {
            self.do_pending_wrap();
        }
    }

    #[cold]
    #[inline(never)]
    fn do_pending_wrap(&mut self) {
        *self.pending_wrap = false;
        let (_col, row) = self.grid.cursor();
        let rows = self.grid.rows();
        if row == *self.scroll_bot {
            self.region_scroll_up(1);
            self.grid.set_cursor(0, row);
        } else if row + 1 < rows {
            self.grid.set_cursor(0, row + 1);
        } else {
            self.grid.set_cursor(0, rows - 1);
        }
        // The row the cursor just flowed onto continues the logical
        // line above — record it so resize can re-wrap instead of
        // truncating.  (For the region-scroll branch the cursor row
        // index is unchanged but the content shifted up; the flag
        // still describes "this row continues the one above it".)
        let (_c, new_row) = self.grid.cursor();
        self.grid.set_row_wrapped(new_row, true);
    }
}

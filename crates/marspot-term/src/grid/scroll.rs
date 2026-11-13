//! Scrolling: the whole screen, which is what files rows into history,
//! and a region inside it.

use super::{Cell, Grid, MAX_FILED_BLANK_RUN, MAX_RESERVED_HEADER_ROWS, PromptMark, RowMark};
use crate::scrollback::RowHint;

impl Grid {
    /// End of a burst of output: lines that scrolled off go to their
    /// files now rather than when a buffer fills.
    pub fn scrollback_hand_off(&self) {
        self.scrollback.hand_off();
    }

    /// Scroll the visible region up by `lines`.  The displaced top rows are
    /// pushed into scrollback (in order, oldest first) and the bottom
    /// `lines` rows are filled with `fill` — pass a Cell carrying the
    /// current SGR background to honor BCE.  The cursor is NOT moved.
    // out of line this cost 1.6 instructions a parsed byte (cat-ascii 47.5 against 45.9)
    #[inline(always)]
    pub fn scroll_up(&mut self, lines: u16, fill: Cell) {
        if lines == 0 {
            return;
        }
        let lines = lines.min(self.rows);
        let cols = self.cols as usize;
        for _ in 0..lines {
            let pr = self.top_row as usize;
            let start = pr * cols;
            // F3+12.1 — back to dumb-store after F3+12 blank-skip
            // turned out to delete legit user blank lines (Enter on
            // empty prompt, intentional paragraph breaks, echo "",
            // ...).  iTerm2 / Alacritty / xterm don't filter and
            // we shouldn't either.  Spinner pollution from TUIs
            // like claudecode is a known trade-off; user can wipe
            // scrollback when it gets unwieldy.  We won't make a
            // policy guess that can't be undone at read time.
            // No blank scan here: this is the per-scrolled-line hot
            // path (`cat` of a large file lands squarely on it), and a
            // 73-cell sweep per line cost 22 % of scroll p99 and 45 %
            // of scroll-cold when it was tried.  The run counter only
            // gates the REGION path, so resetting it is enough — and
            // erring toward "do not collapse" is the safe direction.
            self.filed_blank_run = 0;
            // one row in millions holds a cluster: the test is made
            // here and the calls only for that row
            let row_clusters = if self.clusters.is_empty() {
                Vec::new()
            } else {
                self.degrade_clusters_in_row(start, cols)
            };
            let row_mark = self.marks[pr];
            let end = row_mark.end as usize;
            debug_assert!(
                self.cells[start + end..start + cols].iter().all(|c| *c == Cell::default())
            );
            // the file variant drops a row's blank tail, so it is
            // handed the row without it; the ring wants full rows
            let filed = if self.scrollback.keeps_wrapped_flags() { end } else { cols };
            let hint = RowHint { plain_from: row_mark.plain_from, wide: row_mark.wide };
            // whatever was filed, nothing past the bound needs blanking
            let used = self
                .scrollback
                .push_row(&self.cells[start..start + filed], self.wrapped[pr], hint)
                .min(end);
            let mark = std::mem::take(&mut self.prompt[pr]);
            // The mirrors exist for the in-RAM variant, where there is
            // nowhere else to keep these.  A file-backed pane reads
            // both from the file -- it has to, since a mirror cannot
            // answer for lines written before this process started --
            // so maintaining them there is unbounded growth for an
            // answer nobody asks: measured 2000 entries for 2000 lines
            // of history, and a busy pane reaches hundreds of
            // thousands.
            if !self.scrollback.keeps_wrapped_flags() {
                self.sb_wrapped.push_back(self.wrapped[pr]);
                self.sb_prompt.push_back(mark);
            }
            // Only lines that carry a mark touch the sidecar: one per
            // command, against a stream of millions of lines.
            if mark != PromptMark::None {
                self.scrollback.mark_last_line(mark);
            }
            if !row_clusters.is_empty() || !self.scrollback.keeps_cluster_text() {
                self.keep_row_clusters(row_clusters);
            }
            self.wrapped[pr] = false;
            if fill.is_default() {
                // the cells past what the line used are default
                // already, and the rest is one copy of a blank row
                if row_mark.plain_from == 0 {
                    // nothing in the row but characters ever differed
                    // from a default cell
                    for c in &mut self.cells[start..start + used] {
                        c.ch = ' ';
                    }
                } else {
                    self.cells[start..start + used].copy_from_slice(&self.blank[..used]);
                }
                self.marks[pr] = RowMark::default();
            } else {
                for c in &mut self.cells[start..start + cols] {
                    *c = fill;
                }
                self.marks[pr] = RowMark::unknown(self.cols);
            }
            self.top_row += 1;
            if self.top_row >= self.rows {
                self.top_row = 0;
            }
            self.scroll_push_count = self.scroll_push_count.saturating_add(1);
        }
        // Mirror ring eviction (and the capacity-0 alt-screen case):
        // the flags deque must never outgrow what the ring retains.
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
            self.sb_prompt.pop_front();
        }
        while self.sb_clusters.len() > self.scrollback.len() {
            self.sb_clusters.pop_front();
        }
    }

    /// Monotonic count of lines pushed into scrollback over this
    /// grid's lifetime.  See the field comment for the contract.
    pub fn scroll_push_count(&self) -> u64 {
        self.scroll_push_count
    }

    /// Region-bounded scroll up: shift rows in `top..=bot` upward by
    /// `lines`; new rows at the bottom of the region are blanked with
    /// `fill`. Caller is responsible for keeping `top <= bot < rows`.
    ///
    /// Scrollback is fed **iff the region starts at row 0**.  A region
    /// below a header is a window-internal shuffle and its rows go
    /// nowhere; a region anchored at the top of the screen is one that
    /// reserves rows at the BOTTOM (a TUI's input box and status line),
    /// and content leaving row 0 is leaving the screen upward — the
    /// same event `scroll_up` records.
    ///
    /// This is not a guess.  codex reserves its bottom rows exactly
    /// that way (`CSI 1;56 r`, `CSI 1;58 r`, `CSI 1;53 r` — 582 of
    /// them in one session, with 137 `CSI S`), and dropping those rows
    /// left its panes with **zero** scrollback: the only way back
    /// through the session was codex's own transcript key, which is
    /// slow, jumps, and shows raw uncollapsed output.  claudecode uses
    /// no scroll region at all, which is why the same pane in the same
    /// terminal felt completely different.
    ///
    /// Measured against the reference (2026-09-07): the same sequence
    /// in iTerm2 — region `1..rows-8`, 120 lines scrolled through it —
    /// leaves all 120 reachable in iTerm2's buffer, lines 001 through
    /// 120.  So this matches what the terminal we are chasing does.
    pub fn scroll_up_region(&mut self, top: u16, bot: u16, lines: u16, fill: Cell) {
        if lines == 0 || top > bot || bot >= self.rows {
            return;
        }
        let region_h = bot - top + 1;
        let lines = lines.min(region_h);
        let cols = self.cols as usize;
        // See the doc comment: a region anchored at the top scrolls
        // content off the top of the SCREEN, not merely off the top of
        // a band inside it.
        //
        // "At the top" tolerates a reserved header row.  Strict
        // `top == 0` is what ghostty and xterm use, and it is right for
        // them — they keep no scrollback in the alternate screen at
        // all, so the question never arises there.  We deliberately DO
        // (see `enter_alt_screen`: iTerm2 / Kitty / Alacritty all let
        // the wheel browse a TUI's history), and with that decision
        // already made, one row of chrome should not be what decides
        // whether a session is scrollable.
        //
        // Measured on a real claudecode session (383, 37 MB): it scrolls
        // its transcript with `CSI 2;57 r` + `CSI <n> S` — a region one
        // row down, because it reserves a header line — 1242 times.
        // Under `top == 0` that pane's scrollback peaked at **1 line**;
        // under this rule, at **3523**.  codex uses `CSI 1;56 r` and
        // was always kept.  That single row is the whole reason one
        // agent pane could be scrolled and selected across and the
        // other could not (2026-09-09 field report).
        let feeds_scrollback = top <= MAX_RESERVED_HEADER_ROWS;
        for _ in 0..lines {
            if feeds_scrollback {
                let pr = self.phys_row(top);
                let start = pr * cols;
                let blank = self.cells[start..start + cols]
                    .iter()
                    .all(|c| c.ch == ' ' || c.ch == '\0');
                // A TUI scrolling an empty content area files nothing
                // after the run gets long enough to be chrome rather
                // than spacing — see `MAX_FILED_BLANK_RUN`.  The row
                // still leaves the region below; only the filing stops.
                if !(blank && self.filed_blank_run >= MAX_FILED_BLANK_RUN) {
                    self.filed_blank_run =
                        if blank { self.filed_blank_run.saturating_add(1) } else { 0 };
                    let row_clusters = self.degrade_clusters_in_row(start, cols);
                    let hint = RowHint { plain_from: self.marks[pr].plain_from, wide: self.marks[pr].wide };
                    self.scrollback
                        .push_row(&self.cells[start..start + cols], self.wrapped[pr], hint);
                    let mark = std::mem::take(&mut self.prompt[pr]);
                    if !self.scrollback.keeps_wrapped_flags() {
                        self.sb_wrapped.push_back(self.wrapped[pr]);
                        self.sb_prompt.push_back(mark);
                    }
                    if mark != PromptMark::None {
                        self.scrollback.mark_last_line(mark);
                    }
                    self.keep_row_clusters(row_clusters);
                    self.scroll_push_count = self.scroll_push_count.saturating_add(1);
                }
            }
            // Shift rows [top+1..=bot] up by one logical row.
            for r in top..bot {
                // Copy cells[phys_row(r+1)] → cells[phys_row(r)].
                let src = self.phys_row(r + 1);
                let dst = self.phys_row(r);
                let src_start = src * cols;
                let dst_start = dst * cols;
                // Avoid `&mut` aliasing: copy via an intermediate Vec
                // when src/dst overlap (they don't in this layout, but
                // be defensive).
                let row_data: Vec<Cell> = self.cells[src_start..src_start + cols].to_vec();
                self.cells[dst_start..dst_start + cols].copy_from_slice(&row_data);
                self.marks[dst] = self.marks[src];
            }
            // Continuation flags travel with their rows.
            //
            // They used to be wiped across the whole band before the
            // shift, on the reading that a region scroll is a
            // TUI-internal shuffle where they stop meaning anything.
            // That was written when a region scroll fed no scrollback.
            // Once `feeds_scrollback` arrived, the wipe was running
            // BEFORE the push — so every line a top-anchored region
            // scrolled into history was recorded as "not a
            // continuation", which is all of a codex pane's history.
            // Measured on a live `scrollback.bin`: 1024 lines, zero
            // flags.  A URL that wrapped up there then had a link on
            // its first row and none on its second (2026-09-08 field
            // report).
            for r in top..bot {
                let src = self.phys_row(r + 1);
                let dst = self.phys_row(r);
                self.wrapped[dst] = self.wrapped[src];
            }
            // The row that arrives at the region's top keeps its flag
            // only when the row it continued went to scrollback — then
            // the line above it is still there, one place further
            // back.  In a band below row 0 that predecessor is simply
            // discarded and the row above becomes an unrelated one
            // outside the region, so the flag would be a lie.
            if top > 0 {
                let top_phys = self.phys_row(top);
                self.wrapped[top_phys] = false;
            }
            // Blank the new bottom-of-region row.
            let bot_phys = self.phys_row(bot);
            self.wrapped[bot_phys] = false;
            for c in &mut self.cells[bot_phys * cols..bot_phys * cols + cols] {
                *c = fill;
            }
            self.marks[bot_phys] = RowMark::filled(fill, self.cols);
        }
        // Mirror ring eviction, exactly as `scroll_up` does: the flags
        // deque must never outgrow what the ring retains.
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
            self.sb_prompt.pop_front();
        }
        while self.sb_clusters.len() > self.scrollback.len() {
            self.sb_clusters.pop_front();
        }
    }

    /// Region-bounded scroll down: shift rows in `top..=bot` downward
    /// by `lines`; new rows at the top of the region are blanked with
    /// `fill`. Used by IL (insert line) and CSI T (scroll down).
    pub fn scroll_down_region(&mut self, top: u16, bot: u16, lines: u16, fill: Cell) {
        if lines == 0 || top > bot || bot >= self.rows {
            return;
        }
        let region_h = bot - top + 1;
        let lines = lines.min(region_h);
        let cols = self.cols as usize;
        // Same flag invalidation as scroll_up_region.
        for r in top..=bot {
            let pr = self.phys_row(r);
            self.wrapped[pr] = false;
        }
        for _ in 0..lines {
            // Shift rows [top..bot] down by one logical row.
            for r in (top..bot).rev() {
                let src = self.phys_row(r);
                let dst = self.phys_row(r + 1);
                let src_start = src * cols;
                let dst_start = dst * cols;
                let row_data: Vec<Cell> = self.cells[src_start..src_start + cols].to_vec();
                self.cells[dst_start..dst_start + cols].copy_from_slice(&row_data);
                self.marks[dst] = self.marks[src];
            }
            // Blank the new top-of-region row.
            let top_phys = self.phys_row(top);
            for c in &mut self.cells[top_phys * cols..top_phys * cols + cols] {
                *c = fill;
            }
            self.marks[top_phys] = RowMark::filled(fill, self.cols);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Cell, CellAttrs, Color, Grid};

    fn bottom_row(g: &Grid) -> Vec<Cell> {
        (0..g.cols()).map(|c| g.cell(c, g.rows() - 1)).collect()
    }

    /// The row that comes in at the bottom is the fill from edge to
    /// edge, whatever the row that left held, and at whatever width the
    /// grid has been resized to since.
    #[test]
    fn the_row_scrolled_in_is_the_fill_at_any_width() {
        let red = Cell {
            ch: ' ',
            attrs: CellAttrs { bg: Color::indexed(1), ..CellAttrs::default() },
        };
        let mut g = Grid::with_scrollback(6, 3, 8);
        for (step, cols) in [6u16, 6, 11, 11, 4, 4, 9].into_iter().enumerate() {
            g.resize(cols, 3);
            for r in 0..3 {
                for c in 0..cols {
                    g.set_cell(c, r, Cell::from(if (c + r) % 3 == 0 { ' ' } else { 'x' }));
                }
            }
            let fill = if step % 2 == 0 { Cell::default() } else { red };
            g.scroll_up(1, fill);
            assert_eq!(bottom_row(&g), vec![fill; cols as usize], "step {step}, {cols} columns");
            g.scroll_up(2, Cell::default());
            assert_eq!(bottom_row(&g), vec![Cell::default(); cols as usize]);
        }
    }
}

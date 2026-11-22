//! History that arrives or is read other than by rows scrolling off.

use super::{Cell, Grid};

impl Grid {
    /// RFC-002 §8: contiguous range from scrollback for shelld
    /// `GetScrollbackPage` responses.  Returns each line paired
    /// with its autowrap-continuation flag (`true` = this line was
    /// produced by the parser when the previous line filled the
    /// width and overflowed onto a fresh row, NOT a logical
    /// newline).  Without that flag the receiver's later
    /// `Grid::resize` reflow can't tell logical lines from
    /// hard-wrapped ones and ends up truncating wide content to
    /// the narrowest width the grid ever saw.
    pub fn scrollback_read_page(&self, start: usize, count: usize) -> Vec<(Vec<Cell>, bool)> {
        let cells = self.scrollback.read_lines(start, count);
        let sb_len = self.scrollback.len();
        cells
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                // read_lines returns oldest-first; mirror that into
                // sb_wrapped which is indexed the same way.
                let idx = start + i;
                // Through `scrollback_wrapped`, not the mirror: for a
                // file-backed pane the mirror only holds what this
                // process pushed, so every line written before it
                // started answered "not a continuation" -- and a page
                // of those reflows as truncated content on the
                // receiving side.
                let wrapped = idx < sb_len && self.scrollback_wrapped(idx);
                (row, wrapped)
            })
            .collect()
    }

    /// RFC-002 §8 (step 8c): push a historic line into the tail of
    /// scrollback.  The line must be **older than any line currently
    /// in scrollback** — callers append history in chronological
    /// order before live data has had a chance to scroll anything
    /// off the visible grid.  Once live data starts evicting rows
    /// into scrollback, calling this would corrupt ordering (history
    /// would appear to be newer than already-scrolled-off live
    /// output).  Caller polices the invariant; this is a thin
    /// pass-through.
    ///
    /// `wrapped` is the line's autowrap-continuation flag from the
    /// source (L4 master grid).  Storing it parallel to the cells
    /// is what lets the eventual `reflow` recover the original
    /// logical lines when the user resizes the window — without
    /// it, every historic row looks like a hard newline and a
    /// shrink → grow round trip leaves long lines permanently
    /// chopped.
    pub fn push_historic_scrollback_line(&mut self, line: &[Cell], wrapped: bool) {
        // Go through the wrapped-aware path so the File variant
        // persists the flag in its record; Memory/Disk variants drop
        // the flag (their truth lives in `sb_wrapped` below).
        self.scrollback.push_line_with_wrapped(line, wrapped);
        self.sb_wrapped.push_back(wrapped);
        self.keep_row_extras::<true>(Vec::new());
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
            self.sb_prompt.pop_front();
        }
        self.trim_extras_mirrors::<true>();
    }
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.sb_wrapped.clear();
        self.sb_clusters.clear();
        self.sb_links.clear();
        self.sb_prompt.clear();
    }
}

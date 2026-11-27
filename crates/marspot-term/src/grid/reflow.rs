//! A column-changing resize: gather every line, re-cut it at the new
//! width, redistribute it across history and the screen.
//!
//! Cluster text travels with the cells it belongs to. While a line is
//! being re-cut its cells hold base codepoints -- the form history
//! stores them in, and the one `char_width` can measure -- and the text
//! rides beside them keyed by offset into the logical line. Each row
//! that lands in history files its clusters the way a scrolled-off row
//! does; each row that lands on screen gets them back into the pool.

use super::{Cell, CellAttrs, FLAG_CLUSTER, Grid, PromptMark, char_width, strip_links};

/// A logical line being re-cut, the clusters on it by cell offset,
/// ascending, and what a shell said about it.
struct Line {
    cells: Vec<Cell>,
    clusters: Vec<(usize, String)>,
    prompt: PromptMark,
}

/// One row of the result.
struct Seg {
    cells: Vec<Cell>,
    /// A soft continuation of the row before it.
    cont: bool,
    clusters: Vec<(u16, String)>,
    /// The line's mark, on its first segment only.
    prompt: PromptMark,
}

impl Grid {
    /// Column-changing resize: gather → re-wrap → redistribute.
    pub(super) fn reflow(&mut self, cols: u16, rows: u16) {
        let (mut lines, cursor_line, cursor_off) = self.gather_lines();
        // Trim trailing pure-default cells per logical line, then drop
        // trailing all-blank logical lines that sit BELOW the cursor —
        // they're the unused bottom of the screen, not content.
        for l in lines.iter_mut() {
            // a cluster whose base is a blank still has text to keep
            let keep = l.clusters.last().map_or(0, |(o, _)| o + 1);
            while l.cells.len() > keep && l.cells.last().is_some_and(|c| *c == Cell::default()) {
                l.cells.pop();
            }
        }
        while lines.len() > cursor_line + 1 && lines.last().is_some_and(|l| l.cells.is_empty()) {
            lines.pop();
        }
        let cursor_off = cursor_off.min(lines.get(cursor_line).map_or(0, |l| l.cells.len()));
        let (segs, cursor_seg, cursor_col_new) = rewrap(lines, cols as usize, cursor_line, cursor_off);
        let live_start = segs.len().saturating_sub(rows as usize);
        self.redistribute(segs, cols, rows);

        // Cursor follows its logical position; if its segment got
        // pushed into scrollback (screen shrank under it), clamp to
        // the top-left of the live grid.
        if cursor_seg >= live_start {
            self.cursor_row = (cursor_seg - live_start).min(rows as usize - 1) as u16;
            self.cursor_col = (cursor_col_new as u16).min(cols - 1);
        } else {
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }

    /// Logical lines, oldest first: scrollback, then live rows.  A row
    /// whose continuation flag is set glues onto the previous row.
    /// Returns them with the cursor's (line, offset) within them.
    fn gather_lines(&mut self) -> (Vec<Line>, usize, usize) {
        let old_cols = self.cols as usize;
        let mut lines: Vec<Line> = Vec::new();
        let mut found: Vec<(u16, String)> = Vec::new();
        let mut cursor_line = 0usize;
        let mut cursor_off = 0usize;
        for i in 0..self.scrollback.len() {
            let mut row = self.scrollback.line_to_vec(i).unwrap_or_default();
            // a line read back from the file lost its trailing blanks;
            // a soft wrap on a blank has to glue at the old width
            row.resize(old_cols, Cell::default());
            self.take_history_clusters(i, &mut found);
            let prompt = self.scrollback_prompt(i);
            absorb(&mut lines, row, &mut found, self.scrollback_wrapped(i), prompt);
        }
        for r in 0..self.rows {
            let start = self.phys_row(r) * old_cols;
            let mut row: Vec<Cell> = self.cells[start..start + old_cols].iter().map(|&c| self.expand_cold(c)).collect();
            self.take_live_clusters(&mut row, &mut found);
            let (wrapped, prompt) = (self.wrapped[self.phys_row(r)], self.prompt[self.phys_row(r)]);
            absorb(&mut lines, row, &mut found, wrapped, prompt);
            if r == self.cursor_row {
                cursor_line = lines.len() - 1;
                cursor_off = lines[cursor_line].cells.len() - old_cols + self.cursor_col as usize;
            }
        }
        // every live cluster's text has moved out of the pool
        self.clusters.clear();
        (lines, cursor_line, cursor_off)
    }

    /// The clusters of scrollback line `i` into `out`.  The in-RAM
    /// mirror is about to be rebuilt, so its text is moved, not copied.
    fn take_history_clusters(&mut self, i: usize, out: &mut Vec<(u16, String)>) {
        out.clear();
        if self.scrollback.keeps_cluster_text() {
            self.scrollback.clusters_at(i, out);
        } else if let Some(row) = self.sb_clusters.get_mut(i) {
            out.append(row);
        }
    }

    /// Turn a live row's cluster cells into their base codepoints,
    /// moving each one's text out of the pool into `out`.
    fn take_live_clusters(&mut self, row: &mut [Cell], out: &mut Vec<(u16, String)>) {
        out.clear();
        for (col, cell) in row.iter_mut().enumerate() {
            let Some(idx) = cell.cluster_index() else { continue };
            cell.attrs._pad[0] &= !FLAG_CLUSTER;
            match self.clusters.get_mut(idx as usize).filter(|t| !t.is_empty()) {
                Some(t) => {
                    cell.ch = crate::grapheme::cluster_first_codepoint(t);
                    out.push((col as u16, std::mem::take(t)));
                }
                None => cell.ch = ' ',
            }
        }
    }

    /// The last `rows` segments become the live grid (top-anchored when
    /// everything fits); older ones refill the scrollback at the new
    /// width.
    fn redistribute(&mut self, mut segs: Vec<Seg>, cols: u16, rows: u16) {
        let new_cols = cols as usize;
        let live_start = segs.len().saturating_sub(rows as usize);
        // a file is truncated and rewritten at the new width: bounded by
        // its size, and history never drifts from the display width
        self.scrollback.restart(new_cols);
        self.sb_wrapped.clear();
        self.sb_clusters.clear();
        self.sb_links.clear();
        // each line's mark rides on its first segment: read before the
        // file was restarted, filed again after that segment's row
        self.sb_prompt.clear();
        for i in 0..live_start {
            let mut row = segs[i].cells.clone();
            row.resize(new_cols, pad_cell(i, &segs));
            // history keeps no link ids: their pool is not kept with it
            strip_links(&mut row);
            // Wrapped-aware push so File variant records keep the
            // reflow-derived continuation flag.
            self.scrollback.push_line_with_wrapped(&row, segs[i].cont);
            self.sb_wrapped.push_back(segs[i].cont);
            self.keep_row_prompt(segs[i].prompt);
            self.keep_row_extras::<true>(std::mem::take(&mut segs[i].clusters));
        }
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
            self.sb_prompt.pop_front();
        }
        self.trim_extras_mirrors::<true>();
        let mut new_cells = vec![Cell::default(); new_cols * rows as usize];
        let mut new_wrapped = vec![false; rows as usize];
        let mut new_prompt = vec![PromptMark::None; rows as usize];
        for (r, wrapped) in new_wrapped.iter_mut().enumerate().take(segs.len() - live_start) {
            let i = live_start + r;
            let dst = r * new_cols;
            new_cells[dst..dst + segs[i].cells.len()].copy_from_slice(&segs[i].cells);
            let pad = pad_cell(i, &segs);
            if pad.ch == '\0' {
                for c in &mut new_cells[dst + segs[i].cells.len()..dst + new_cols] {
                    *c = pad;
                }
            }
            for (col, text) in std::mem::take(&mut segs[i].clusters) {
                let at = dst + col as usize;
                // text read back from a file may be a lone codepoint
                let mut cs = text.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) => new_cells[at].ch = c,
                    (Some(_), Some(_)) => new_cells[at] = self.cluster_cell(&text, new_cells[at].attrs),
                    _ => {}
                }
            }
            *wrapped = segs[i].cont;
            new_prompt[r] = segs[i].prompt;
        }
        self.replace_cells(cols, rows, &new_cells);
        self.prompt = new_prompt;
        self.wrapped = new_wrapped;
        self.cols = cols;
        self.rows = rows;
        self.top_row = 0;
        self.forget_row_marks();
    }
}

/// Add one row to the logical lines, taking `found` -- its clusters by
/// column -- along with it.
fn absorb(lines: &mut Vec<Line>, row: Vec<Cell>, found: &mut Vec<(u16, String)>, wrapped: bool, prompt: PromptMark) {
    // clusters come from files and snapshots: keep those inside the
    // row, in column order, which `rewrap` relies on
    found.retain(|(c, _)| (*c as usize) < row.len());
    found.sort_by_key(|(c, _)| *c);
    if wrapped && let Some(prev) = lines.last_mut() {
        // Drop wide-bump pad sentinels at the glue seam: trailing NULs
        // that are NOT the trail half of a wide lead.  (A legit trail
        // NUL always directly follows a width-2 char or cluster -- a
        // VS16 cluster is wide while its base codepoint is not.)
        let cells = &mut prev.cells;
        loop {
            let n = cells.len();
            if n == 0 || cells[n - 1].ch != '\0' {
                break;
            }
            let lead = n.wrapping_sub(2);
            if n >= 2
                && (char_width(cells[lead].ch) == 2
                    || prev.clusters.iter().any(|(o, t)| *o == lead && crate::grapheme::cluster_width(t) == 2))
            {
                break;
            }
            cells.pop();
        }
        let at = cells.len();
        cells.extend(row);
        prev.clusters.extend(found.drain(..).map(|(c, t)| (at + c as usize, t)));
        // a prompt that starts on a continuation row still starts this line
        if prompt == PromptMark::PromptStart || prev.prompt == PromptMark::None {
            prev.prompt = prompt;
        }
    } else {
        let clusters = found.drain(..).map(|(c, t)| (c as usize, t)).collect();
        lines.push(Line { cells: row, clusters, prompt });
    }
}

/// Cut each line into segments of `new_cols`, never splitting a wide
/// pair (lead + NUL trail) across the boundary, and say where the
/// cursor's (line, offset) lands as (segment, column).
fn rewrap(lines: Vec<Line>, new_cols: usize, cursor_line: usize, cursor_off: usize) -> (Vec<Seg>, usize, usize) {
    let mut segs: Vec<Seg> = Vec::new();
    let mut cursor_seg = 0usize;
    let mut cursor_col_new = 0usize;
    for (li, line) in lines.into_iter().enumerate() {
        let mut clusters = line.clusters.into_iter().peekable();
        let mut prompt = line.prompt;
        let line = line.cells;
        let mut start = 0usize;
        let mut first = true;
        loop {
            let remaining = line.len() - start;
            let mut take = remaining.min(new_cols);
            // Wide pair straddling the boundary: the trail NUL would
            // land at the start of the next segment.  Pull the lead
            // over instead (its column renders blank, exactly like the
            // emulator's deferred-wrap print).
            if take > 0 && take < remaining && line[start + take].ch == '\0' {
                take -= 1;
            }
            // Degenerate 1-column grid with a wide pair: splitting is
            // the only way to make progress.
            if take == 0 && remaining > 0 {
                take = 1;
            }
            let end = start + take;
            if li == cursor_line
                && cursor_off >= start
                && (cursor_off < end || (cursor_off == end && remaining <= new_cols))
            {
                cursor_seg = segs.len();
                cursor_col_new = cursor_off - start;
            }
            let mut here = Vec::new();
            while let Some((off, text)) = clusters.next_if(|(off, _)| *off < end) {
                here.push(((off - start) as u16, text));
            }
            segs.push(Seg { cells: line[start..end].to_vec(), cont: !first, clusters: here, prompt });
            prompt = PromptMark::None;
            first = false;
            start = end;
            if start >= line.len() {
                break;
            }
        }
    }
    if segs.is_empty() {
        segs.push(Seg { cells: Vec::new(), cont: false, clusters: Vec::new(), prompt: PromptMark::None });
    }
    (segs, cursor_seg, cursor_col_new)
}

/// A fragment that underfills its row because the next wide char was
/// bumped to the following segment pads its tail with NUL sentinels —
/// the next gather drops them at the seam instead of treating them as
/// spaces.
fn pad_cell(i: usize, segs: &[Seg]) -> Cell {
    if segs.get(i + 1).is_some_and(|s| s.cont) {
        Cell { ch: '\0', attrs: CellAttrs::default() }
    } else {
        Cell::default()
    }
}

//! Reading history back out.
//!
//! A read is served from the RAM ring when it can be and from the
//! file when it cannot; the caller cannot tell which, and a read
//! that finds nothing returns `None` rather than guessing.

use super::*;

impl FileScrollback {
    fn read_idx_via_mmap(&self, line_idx: u64) -> std::io::Result<u64> {
        let off_in_idx = line_idx * 8;
        self.ensure_idx_mmap_covers(off_in_idx + 8)?;
        let mmap_ptr = self.idx_mmap_ptr.get();
        let mmap_len = self.idx_mmap_len.get();
        if !mmap_ptr.is_null() && (off_in_idx as usize) + 8 <= mmap_len {
            let buf = unsafe { std::slice::from_raw_parts(mmap_ptr.add(off_in_idx as usize), 8) };
            return Ok(u64::from_le_bytes(buf.try_into().unwrap()));
        }
        // Fallback (rare): pread.
        read_idx_at(&self.idx_for_read, line_idx)
    }

    pub fn is_empty(&self) -> bool {
        self.total_lines == 0
    }

    pub fn len(&self) -> usize {
        self.total_lines as usize
    }

    pub fn capacity(&self) -> usize {
        // No eviction in v1 → conceptually unbounded; we report the
        // RAM ring size so `approx_bytes` and bench harnesses match
        // the same convention as MemoryScrollback.
        self.ram_capacity
    }

    /// The command mark on logical line `line_idx`, or `None` when
    /// there is no mark there to find.
    ///
    /// Both tiers: the hot file's sidecar, and the one that was renamed
    /// alongside the file when it was handed over. Older than the cold
    /// tier is gone with its file and answers "no mark" -- the same
    /// answer a missing sidecar gives.
    pub(in crate::scrollback) fn mark_at(&self, line_idx: usize) -> crate::grid::PromptMark {
        let none = crate::grid::PromptMark::None;
        if line_idx >= self.total_lines as usize {
            return none;
        }
        let (file, local) = if (line_idx as u64) < self.hot_first_line {
            let Some(l) = (line_idx as u64).checked_sub(self.cold_first_line) else {
                return none;
            };
            if l >= self.cold_total_lines {
                return none;
            }
            (self.cold_extras.marks.as_ref(), l)
        } else {
            (self.extras.marks.as_ref(), line_idx as u64 - self.hot_first_line)
        };
        let Some(f) = file else { return none };
        Self::read_mark(f, local)
    }

    /// The clusters on logical line `line_idx`, appended to `out`.
    ///
    /// Same two tiers and same arithmetic as [`Self::mark_at`] -- a
    /// line older than the cold file has gone with it and has no
    /// clusters, which is the answer a missing sidecar gives too, and
    /// reads on screen as the base codepoints the record itself holds.
    pub(in crate::scrollback) fn clusters_at(
        &self,
        line_idx: usize,
        out: &mut Vec<(u16, String)>,
    ) -> bool {
        if line_idx >= self.total_lines as usize {
            return false;
        }
        let (extras, local) = if (line_idx as u64) < self.hot_first_line {
            let Some(l) = (line_idx as u64).checked_sub(self.cold_first_line) else {
                return false;
            };
            if l >= self.cold_total_lines {
                return false;
            }
            (&self.cold_extras, l)
        } else {
            (&self.extras, line_idx as u64 - self.hot_first_line)
        };
        extras.read_clusters(local, out)
    }

    fn read_mark(f: &std::fs::File, local: u64) -> crate::grid::PromptMark {
        use std::os::unix::fs::FileExt;
        let mut b = [0u8; 2];
        if f.read_exact_at(&mut b, super::super::sidecar::HEADER_BYTES + local * 2)
            .is_err()
        {
            return crate::grid::PromptMark::None;
        }
        crate::grid::PromptMark::from_bytes(b)
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<crate::grid::Cell> {
        if line_idx >= self.total_lines as usize {
            return None;
        }
        let ram_first = (self.total_lines as usize).saturating_sub(self.ram_len);
        if line_idx >= ram_first {
            // In RAM ring.
            let ring_idx = line_idx - ram_first;
            let slot = (self.ram_head + ring_idx) % self.ram_capacity.max(1);
            let start = slot * self.cols;
            // Past what this slot actually holds is a blank, not the
            // previous occupant's cell.
            if col >= self.ram_lens.get(slot).copied().unwrap_or(0) as usize {
                return Some(crate::grid::Cell::default());
            }
            return self.ram_cells.get(start + col).copied();
        }
        // F2+5 — when the requested line is older than hot's first row,
        // try the cold tier.  Lines older than cold_first_line are
        // gone (delete tier — overwritten by a previous rotation).
        if (line_idx as u64) < self.hot_first_line {
            if (line_idx as u64) < self.cold_first_line {
                return None;
            }
            return self.cold_cell_at(line_idx as u64, col);
        }
        // Hot tier — flush BufWriters so any line that aged out of the
        // ring is on disk, then read from the mmap tier.  The first
        // cold-read in a session pays the flush + mmap (one-shot
        // cost); subsequent cold reads in the same burst are pure
        // memory accesses.
        self.ensure_flushed();
        // Translate logical → hot-local idx.
        let hot_local = (line_idx as u64) - self.hot_first_line;
        let off = self.read_idx_via_mmap(hot_local).ok()?;
        // BUGFIX (F2+3) — was `cells.into_iter().next()` which returns
        // cell 0 of the row regardless of `col`, so every column of an
        // off-ring scrollback line rendered as the row's first
        // character (the "row of repeated chars" corruption pattern
        // visible after F1+13 dropped RAM ring 1024→256, which made
        // the mmap path the common case).  The pread fallback below
        // already used `cells.get(col)`; this aligns the primary mmap
        // path with it.
        self.read_record_via_mmap(off, Some(col))
            .and_then(|(cells, _w)| cells.get(col).copied())
            .or_else(|| {
                // Fallback if mmap path failed for any reason: classic
                // pread.  Same correctness; just slower.
                let (cells, _w) = read_record_at(&self.bin_for_read, off).ok()?;
                cells.get(col).copied()
            })
    }

    /// F2+5 — read one cell from the cold tier via pread.  Cold is
    /// not mmap'd (the access frequency is low — old scroll-back and
    /// search-only; pay the pread per-call instead of paying the
    /// mmap + remap accounting).  Returns None if cold isn't open
    /// or the requested line_idx isn't in cold's range.
    fn cold_cell_at(&self, line_idx: u64, col: usize) -> Option<crate::grid::Cell> {
        let cold_local = line_idx.checked_sub(self.cold_first_line)?;
        if cold_local >= self.cold_total_lines {
            return None;
        }
        let cold_idx = self.cold_idx_for_read.as_ref()?;
        let cold_bin = self.cold_bin_for_read.as_ref()?;
        let off = read_idx_at(cold_idx, cold_local).ok()?;
        let (cells, _w) = read_record_at(cold_bin, off).ok()?;
        cells.get(col).copied()
    }

    /// Read a single record from the bin mmap.  `col_filter` is an
    /// optimisation: when Some(col) we still decode the whole line
    /// (because cell offsets within the record are positional), but
    /// the caller can pick the column it wanted.  Returning the
    /// whole `Vec<Cell>` keeps the API simple; v1's cold-read
    /// budget already absorbs the per-line decode cost.
    fn read_record_via_mmap(
        &self,
        offset: u64,
        _col_filter: Option<usize>,
    ) -> Option<(Vec<crate::grid::Cell>, bool)> {
        // Ensure mmap covers at least the rec_len header.
        self.ensure_bin_mmap_covers(offset + 4).ok()?;
        let mmap_ptr = self.bin_mmap_ptr.get();
        let mmap_len = self.bin_mmap_len.get();
        if mmap_ptr.is_null() || (offset as usize) + 4 > mmap_len {
            return None;
        }
        let len_slice = unsafe { std::slice::from_raw_parts(mmap_ptr.add(offset as usize), 4) };
        let rec_len = u32::from_le_bytes(len_slice.try_into().unwrap()) as usize;
        // Extend mmap if record's body lives past current end.
        if (offset as usize) + 4 + rec_len > mmap_len {
            self.ensure_bin_mmap_covers(offset + 4 + rec_len as u64)
                .ok()?;
        }
        let mmap_ptr = self.bin_mmap_ptr.get();
        let mmap_len = self.bin_mmap_len.get();
        if (offset as usize) + 4 + rec_len > mmap_len {
            return None;
        }
        let body =
            unsafe { std::slice::from_raw_parts(mmap_ptr.add(offset as usize + 4), rec_len) };
        if body.len() < 3 {
            return None;
        }
        let wrapped = body[0] != 0;
        let cols = u16::from_le_bytes([body[1], body[2]]) as usize;
        let cells = decode_record_cells(&body[3..], cols)?;
        Some((cells, wrapped))
    }

    pub fn read_line(&self, idx: usize) -> Option<Vec<crate::grid::Cell>> {
        if idx >= self.total_lines as usize {
            return None;
        }
        let ram_first = (self.total_lines as usize).saturating_sub(self.ram_len);
        if idx >= ram_first {
            let ring_idx = idx - ram_first;
            let slot = (self.ram_head + ring_idx) % self.ram_capacity.max(1);
            let start = slot * self.cols;
            let used = self.ram_lens.get(slot).copied().unwrap_or(0) as usize;
            let mut row = self.ram_cells[start..start + used.min(self.cols)].to_vec();
            row.resize(self.cols, crate::grid::Cell::default());
            return Some(row);
        }
        // F2+5 — cold tier fallthrough.
        if (idx as u64) < self.hot_first_line {
            if (idx as u64) < self.cold_first_line {
                return None;
            }
            let cold_local = (idx as u64) - self.cold_first_line;
            let cold_idx = self.cold_idx_for_read.as_ref()?;
            let cold_bin = self.cold_bin_for_read.as_ref()?;
            let off = read_idx_at(cold_idx, cold_local).ok()?;
            let (cells, _) = read_record_at(cold_bin, off).ok()?;
            return Some(cells);
        }
        self.ensure_flushed();
        let hot_local = (idx as u64) - self.hot_first_line;
        let off = self.read_idx_via_mmap(hot_local).ok()?;
        self.read_record_via_mmap(off, None)
            .map(|(c, _)| c)
            .or_else(|| {
                let (cells, _wrapped) = read_record_at(&self.bin_for_read, off).ok()?;
                Some(cells)
            })
    }

    pub fn wrapped_at(&self, idx: usize) -> bool {
        if idx >= self.total_lines as usize {
            return false;
        }
        let ram_first = (self.total_lines as usize).saturating_sub(self.ram_len);
        if idx >= ram_first {
            let ring_idx = idx - ram_first;
            let slot = (self.ram_head + ring_idx) % self.ram_capacity.max(1);
            return self.ram_wrapped.get(slot).copied().unwrap_or(false);
        }
        // F2+5 — cold tier fallthrough.
        if (idx as u64) < self.hot_first_line {
            if (idx as u64) < self.cold_first_line {
                return false;
            }
            let Some(cold_idx_fd) = self.cold_idx_for_read.as_ref() else {
                return false;
            };
            let Some(cold_bin_fd) = self.cold_bin_for_read.as_ref() else {
                return false;
            };
            let cold_local = (idx as u64) - self.cold_first_line;
            let Ok(off) = read_idx_at(cold_idx_fd, cold_local) else {
                return false;
            };
            let Ok((_, wrapped)) = read_record_at(cold_bin_fd, off) else {
                return false;
            };
            return wrapped;
        }
        self.ensure_flushed();
        let hot_local = (idx as u64) - self.hot_first_line;
        let Some(off) = self.read_idx_via_mmap(hot_local).ok() else {
            return false;
        };
        if let Some((_, w)) = self.read_record_via_mmap(off, None) {
            return w;
        }
        let Some((_cells, wrapped)) = read_record_at(&self.bin_for_read, off).ok() else {
            return false;
        };
        wrapped
    }

    pub fn clear(&mut self) {
        // CSI 3 J = the user's EXPLICIT "wipe my history" instruction
        // — it must reach the persistent tier, or `clear` + app
        // restart resurrects everything from disk (2026-07-17 field
        // report: "clear 了,重开还是一大堆 history").  The old
        // behaviour cleared only the RAM ring "in case the user
        // wanted to keep the file"; that guess inverted the actual
        // contract (RFC-004 invariant 4: destructive actions happen
        // exactly when the user explicitly asks — and 3J is exactly
        // that ask).  The bytelog is NOT touched: it's the disaster-
        // recovery ground truth, and replaying it re-applies this
        // very 3J, converging on the same cleared state.
        self.ram_cells.clear();
        self.ram_wrapped.clear();
        self.ram_head = 0;
        self.ram_len = 0;
        // Truncate the hot pair to an empty (header-only) state.
        // `truncate` runs on the writer thread behind everything
        // already queued, and drops what is still buffered — records
        // from before the clear must not resurrect past the new end.
        // The fds are append-mode, so later pushes land at the new EOF.
        self.bin.borrow_mut().truncate(FILE_HEADER_BYTES);
        self.idx.borrow_mut().truncate(0);
        self.bin_tail_offset = FILE_HEADER_BYTES;
        self.total_lines = 0;
        self.hot_first_line = 0;
        // The header survives the truncate, so the epoch in it would
        // too -- and the next lines pushed take indices from zero
        // again, which puts this in reflow's situation: a local line
        // number comes back meaning a different line.  Without a new
        // one, a mark filed against the cleared line 4 is handed back
        // for the new line 4, and a jump lands on it.
        let fresh = super::super::format::new_epoch();
        self.bin.borrow_mut().flush();
        if super::super::format::stamp_epoch_in_place(&self.bin_path, fresh).is_ok() {
            self.epoch = fresh;
            self.extras = super::super::sidecar::LineExtras::open_hot(&self.bin_path, fresh);
        }
        // Drop the cold tier entirely.
        self.cold_bin_for_read = None;
        self.cold_idx_for_read = None;
        self.cold_first_line = 0;
        self.cold_total_lines = 0;
        let _ = std::fs::remove_file(&self.cold_bin_path);
        let _ = std::fs::remove_file(&self.cold_idx_path);
        self.cold_extras = super::super::sidecar::LineExtras::none();
        super::super::sidecar::LineExtras::remove_beside(&self.cold_bin_path);
        // Invalidate read-side mmaps — they cover pre-truncate bytes.
        let bp = self.bin_mmap_ptr.get();
        let bl = self.bin_mmap_len.get();
        if !bp.is_null() && bl > 0 {
            unsafe {
                libc::munmap(bp as *mut libc::c_void, bl);
            }
        }
        self.bin_mmap_ptr.set(std::ptr::null_mut());
        self.bin_mmap_len.set(0);
        let ip = self.idx_mmap_ptr.get();
        let il = self.idx_mmap_len.get();
        if !ip.is_null() && il > 0 {
            unsafe {
                libc::munmap(ip as *mut libc::c_void, il);
            }
        }
        self.idx_mmap_ptr.set(std::ptr::null_mut());
        self.idx_mmap_len.set(0);
        self.has_unflushed.set(false);
    }

    pub fn approx_bytes(&self) -> usize {
        // RAM ring resident bytes (matches Memory/Disk approx_bytes
        // semantics — what we hold in process, not what's on disk).
        self.ram_len * self.cols * crate::grid::CELL_MEM_BYTES
    }

}
impl FileScrollback {
    /// B3 — flush the live writer's BufWriters and hand back a
    /// `FileSnapshot` that the search worker can pread independently.
    /// The new fds are opened against the same `.bin` / `.idx` paths
    /// the live writer is appending to; pread on the snapshot's fds
    /// never blocks the writer and the writer never invalidates the
    /// snapshot's view (appends only grow the file past
    /// `total_lines`).
    pub fn snapshot_for_search(&self) -> std::io::Result<FileSnapshot> {
        self.bin.borrow_mut().flush();
        self.idx.borrow_mut().flush();
        let bin = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.bin_path)?;
        let idx = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.idx_path)?;
        Ok(FileSnapshot::from_fds(bin, idx, self.total_lines))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    use crate::scrollback::testing::*;

    /// Both cold read paths — the mmap one behind `cell_at` and the
    /// pread one behind `read_line` — decode a file whose records are a
    /// mix of both widths.
    #[test]
    fn both_read_paths_decode_a_file_of_mixed_widths() {
        let tmp = TmpDir::new("mixed-widths");
        let cols = 10;
        let old: Vec<(Vec<Cell>, bool)> = (0..8).map(|i| (rich_row(i, cols), false)).collect();
        write_v2_pair(&tmp.bin(), &tmp.idx(), &old);
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 2).expect("open");
        let new: Vec<(Vec<Cell>, bool)> = (50..58).map(|i| (rich_row(i, cols), false)).collect();
        for (row, w) in &new {
            sb.push_line(row, *w);
        }
        let all: Vec<_> = old.iter().chain(new.iter()).collect();
        // Everything but the last two lines is off the RAM ring.
        for (i, (row, _)) in all.iter().enumerate().take(all.len() - 2) {
            for (c, want) in row.iter().enumerate() {
                assert_eq!(
                    sb.cell_at(i, c).as_ref(),
                    Some(want),
                    "cell_at line {i} col {c}"
                );
            }
            assert_eq!(&sb.read_line(i).unwrap()[..], &row[..], "read_line {i}");
        }
    }

    #[test]
    fn file_create_then_roundtrip_one_line() {
        let tmp = TmpDir::new("one-line");
        let cols = 8usize;
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16).expect("create");
        let line = fill(b'a', cols);
        sb.push_line(&line, false);
        assert_eq!(sb.len(), 1);
        let got = sb.read_line(0).expect("read");
        assert_eq!(got.len(), cols);
        assert_eq!(got[0].ch, 'a');
        assert_eq!(sb.cell_at(0, 3).unwrap().ch, 'a');
        assert!(!sb.wrapped_at(0));
    }

    /// REGRESSION (F2+3) — `cell_at(line, col)` must return the cell at
    /// `col`, NOT the row's first cell, when the line has aged out of
    /// the RAM ring and is served by the file mmap path.  The original
    /// A2 cold-read code path used `cells.into_iter().next()` after
    /// decoding the whole row, which silently returned cell 0 for every
    /// column.  Hit thresholds: line index < (total_lines - ram_capacity).
    /// On real users this manifested as scrollback rows rendered as
    /// "the row's first char repeated N times" (e.g. 'eeee…', 'aaaa…').
    /// Caught after F1+13 dropped ram_capacity 1024→256, so the mmap
    /// path became the steady-state for daily-use depths.
    #[test]
    fn cell_at_off_ring_returns_correct_column() {
        let tmp = TmpDir::new("off-ring-col");
        let cols = 8usize;
        let ram_cap = 4usize; // tiny so eviction is easy
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_cap).expect("create");
        // Build a row whose first char != other chars so the bug
        // (return cell 0 for every col) is observable.
        let mut row0: Vec<Cell> = Vec::with_capacity(cols);
        for c in 0..cols {
            row0.push(Cell {
                ch: (b'A' + c as u8) as char,
                ..Default::default()
            });
        }
        sb.push_line(&row0, false);
        // Push enough fillers to evict row 0 from the RAM ring.
        for _ in 0..(ram_cap + 4) {
            sb.push_line(&fill(b'.', cols), false);
        }
        // Row 0 now lives only on disk; cell_at(0, col) must take the
        // mmap path.  Assert each column returns its own letter.
        for c in 0..cols {
            let got = sb
                .cell_at(0, c)
                .unwrap_or_else(|| panic!("cell_at(0, {}) returned None", c));
            let want = (b'A' + c as u8) as char;
            assert_eq!(
                got.ch, want,
                "cell_at(0, {}) returned {:?}, expected {:?} \
                 (regression: mmap path returning cell 0 for every col)",
                c, got.ch, want
            );
        }
    }

    #[test]
    fn file_wrapped_flag_survives_roundtrip() {
        let tmp = TmpDir::new("wrapped");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("create");
            sb.push_line(&fill(b'p', cols), false);
            sb.push_line(&fill(b'q', cols), true);
            sb.push_line(&fill(b'r', cols), false);
            sb.push_line(&fill(b's', cols), true);
        }
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("reopen");
        assert_eq!(sb.len(), 4);
        assert!(!sb.wrapped_at(0));
        assert!(sb.wrapped_at(1));
        assert!(!sb.wrapped_at(2));
        assert!(sb.wrapped_at(3));
    }

    /// A2: cold reads of lines that aged out of the RAM ring but
    /// might still sit in the BufWriter buffer must trigger an
    /// explicit flush + mmap before reading.  Without this, reads of
    /// such lines would either miss bytes (pread sees only flushed)
    /// or miss mmap coverage entirely.  Use a NARROW cols so the
    /// 64-KiB BufWriter swallows lots of records and the ring rolls
    /// before the buffer naturally overflows.
    #[test]
    fn file_a2_cold_read_triggers_flush_and_mmap() {
        let tmp = TmpDir::new("a2-cold");
        let cols = 4usize; // ~59 bytes/record -> ~1100 fit in 64 KiB
        let ram_capacity = 16; // tiny ring
        let mut sb =
            FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_capacity).expect("create");
        // Push enough lines so the ring has rolled many times but
        // the BufWriter still has not auto-flushed.
        let n = 200usize; // 200 * 59 = 11.8 KiB < 64 KiB buffer
        for i in 0..n {
            let ch = b'a' + (i % 26) as u8;
            sb.push_line(&fill(ch, cols), false);
        }
        assert_eq!(sb.len(), n);
        // Recent lines: should be in RAM ring, no IO needed.
        let recent_idx = n - 1;
        let ch_recent = (b'a' + (recent_idx % 26) as u8) as char;
        assert_eq!(sb.cell_at(recent_idx, 0).unwrap().ch, ch_recent);
        // Old line: must have aged out of the ring.  Bytes are
        // still in the BufWriter buffer.  cell_at must flush + read
        // correctly.
        let old_idx = 5;
        let ch_old = (b'a' + (old_idx % 26) as u8) as char;
        assert_eq!(
            sb.cell_at(old_idx, 0).expect("old cell").ch,
            ch_old,
            "cold read of pre-buffer line must flush + read correctly"
        );
        // After cold read: has_unflushed should be back to false.
        assert!(!sb.has_unflushed.get(), "ensure_flushed should clear flag");
        // mmap should be populated.
        assert!(!sb.bin_mmap_ptr.get().is_null(), "bin mmap should be set");
        assert!(!sb.idx_mmap_ptr.get().is_null(), "idx mmap should be set");
    }
}

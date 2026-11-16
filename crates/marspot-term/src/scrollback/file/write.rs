//! Appending lines, and changing segments when the hot one is full.
//!
//! The write side runs on the parse thread, so what is here is the
//! per-line cost of keeping history: no allocation per line, no fsync,
//! no system call -- a segment change is arithmetic and a cut in the
//! writer's stream, and the segment it changes to was made ready
//! elsewhere.

use std::sync::Arc;

use super::tiers::SegTier;
use super::*;
use crate::async_writer::Target;
use crate::segment::{self, Pending};

/// What a clear does to the files, in order.  The older segments are
/// put out of use before the hot one, so that a clear cut short leaves
/// the hot segment's lines or none -- never the older ones'.
#[cfg(test)]
pub(super) const CLEAR_STEPS: usize = 2;

impl FileScrollback {
    /// Append one line.  Hot path.
    ///
    /// Returns the line's length without its trailing default cells.
    pub fn push_line(&mut self, line: &[crate::grid::Cell], wrapped: bool) -> usize {
        self.push_row(line, wrapped, RowHint::NONE)
    }

    /// [`Self::push_line`], with what the caller knows of the row.
    pub fn push_row(&mut self, line: &[crate::grid::Cell], wrapped: bool, hint: RowHint) -> usize {
        // Trailing cells equal to `Cell::default()` are not stored: a
        // read past a record's columns is a blank, which looks the same,
        // and a wide pane of short lines is mostly such cells.  Compared
        // as three words, which for `Cell` is the same as `==`: the
        // field-wise compare cost ~29 instructions a cell, a fifth of all
        // the parse thread did on a wide pane of short lines.
        const N: usize = crate::grid::CELL_MEM_BYTES;
        fn words(c: &[u8; N]) -> (u64, u64, u32) {
            let (a, rest) = c.split_first_chunk::<8>().expect("20 bytes");
            let (b, rest) = rest.split_first_chunk::<8>().expect("12 bytes");
            let c = rest.first_chunk::<4>().expect("4 bytes");
            (u64::from_ne_bytes(*a), u64::from_ne_bytes(*b), u32::from_ne_bytes(*c))
        }
        let default_cell = crate::grid::Cell::default();
        let blank = words(
            crate::grid::Cell::slice_as_bytes(std::slice::from_ref(&default_cell))
                .try_into()
                .expect("a cell is CELL_MEM_BYTES"),
        );
        let trimmed_len = crate::grid::Cell::slice_as_bytes(line)
            .as_chunks::<N>()
            .0
            .iter()
            .rposition(|c| words(c) != blank)
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &line[..trimmed_len];
        let cell_bytes = crate::grid::Cell::slice_as_bytes(line);
        // The record is the header plus the line as runs of cells that
        // share their attributes, or, for the rare line where that is
        // no smaller, the row's own memory.  See FILE_VERSION.
        self.scratch.clear();
        self.scratch.extend_from_slice(&[0u8; FILE_REC_HEADER_BYTES]);
        let packed = !line.is_empty() && encode_packed_with(line, hint, &mut self.scratch);
        if !packed {
            self.scratch.extend_from_slice(cell_bytes);
        }
        self.file_record(line.len(), wrapped, packed)
    }

    /// [`Self::push_row`] for a row of the grid's own cells, read
    /// through its table.  The record is byte for byte the one
    /// `push_row` writes for the same row expanded.
    pub fn push_packed_row(
        &mut self,
        line: &[crate::grid::PackedCell],
        styles: &crate::grid::StyleTable,
        wrapped: bool,
        hint: RowHint,
    ) -> usize {
        let trimmed_len =
            line.iter().rposition(|c| *c != crate::grid::PackedCell::BLANK).map_or(0, |i| i + 1);
        let line = &line[..trimmed_len];
        self.scratch.clear();
        self.scratch.extend_from_slice(&[0u8; FILE_REC_HEADER_BYTES]);
        let packed = !line.is_empty()
            && crate::scrollback::format_packed::encode_packed_cells(line, styles, hint, &mut self.scratch, &mut self.canon);
        if !packed {
            crate::scrollback::format_packed::append_cells(line, styles, &mut self.scratch);
        }
        self.file_record(line.len(), wrapped, packed)
    }

    /// Write the record in `scratch`, whose body is in place: fill in
    /// its header, append it, and keep it in the ring.  Returns `cols`.
    fn file_record(&mut self, cols: usize, wrapped: bool, packed: bool) -> usize {
        let trimmed_len = cols;
        let cols_u16 = cols as u16;
        let total_bytes = self.scratch.len() as u64;
        let rec_len = (total_bytes - 4) as u32;
        let flags = if wrapped { REC_WRAPPED } else { 0 } | if packed { REC_PACKED } else { 0 };
        self.scratch[..4].copy_from_slice(&rec_len.to_le_bytes());
        self.scratch[4] = flags;
        self.scratch[5..7].copy_from_slice(&cols_u16.to_le_bytes());

        if self.hot_pos + total_bytes > self.mark || self.hot_lines >= self.line_mark {
            self.at_mark(total_bytes);
        }
        let entry = self.hot_pos | (self.epoch << 32);
        self.bin.borrow_mut().write(&self.scratch);
        self.bin.borrow_mut().write_side(&entry.to_le_bytes());
        self.hot_pos += total_bytes;
        self.hot_lines += 1;
        self.has_unflushed.set(true);

        self.ring.push(&self.scratch[4..]);
        self.total_lines += 1;
        trimmed_len
    }

    /// Where the hot segment's record area ends.
    pub(super) fn hot_data_end(&self) -> u64 {
        self.segs.last().expect("a hot segment").seg.hdr.index_at()
    }

    fn hot_line_cap(&self) -> u64 {
        self.segs.last().expect("a hot segment").seg.hdr.index / 8
    }

    /// The record bytes of the segment the next change takes, or `None`
    /// when it uses the older full one again.
    pub(super) fn next_size(&self) -> Option<u64> {
        let hot = self.segs.last().expect("a hot segment").seg.hdr.data;
        if hot < self.sizes.full {
            return Some(segment::grown(hot, self.sizes.full));
        }
        let full = self.segs.iter().filter(|t| t.seg.hdr.data == self.sizes.full).count();
        (full < 2).then_some(self.sizes.full)
    }

    /// Where the next push stops: where the next segment is asked for,
    /// if it is still to be asked for, or else the end.
    pub(super) fn set_marks(&mut self) {
        let (end, cap) = (self.hot_data_end(), self.hot_line_cap());
        let ask = self.next_size().is_some()
            && self.pending.is_none()
            && !self.spares.iter().any(|s| Some(s.hdr.data) == self.next_size());
        if ask {
            let data = end - HEADER_BYTES;
            let ahead = segment::ask_ahead(data);
            self.mark = end - ahead;
            // the same share of the index
            self.line_mark = cap - cap * ahead / data;
        } else {
            self.mark = end;
            self.line_mark = cap;
        }
    }

    /// The record about to be written takes the hot segment past a mark:
    /// where its successor is asked for, or its end.
    #[cold]
    fn at_mark(&mut self, record_bytes: u64) {
        if self.pending.is_none()
            && let Some(data) = self.next_size()
            && !self.spares.iter().any(|s| s.hdr.data == data)
        {
            self.pending = Some(Pending::start(self.prep(data)));
        }
        self.set_marks();
        if self.hot_pos + record_bytes > self.hot_data_end() || self.hot_lines >= self.hot_line_cap() {
            self.change();
        }
    }

    fn prep(&mut self, data: u64) -> segment::Prep {
        let slot = self.next_slot;
        self.next_slot += 1;
        #[allow(unused_mut)]
        let mut prep = segment::Prep::new(&self.dir, &self.stem, slot, open::header(data, 0));
        #[cfg(test)]
        {
            if let Some(bytes) = self.hooks.prep_bytes.take() {
                prep.hdr.data = bytes;
            }
            prep.upto = self.hooks.prep_upto.unwrap_or(prep.upto);
            prep.gate = self.hooks.prep_gate.take();
        }
        prep
    }

    /// Go on in the next segment: the older full one used again, or the
    /// one made ready, waited for if it is not ready yet -- the history
    /// inside the budget is not given up for it.  A segment that could
    /// not be made (a full disk) leaves the history going round in the
    /// segments it has.
    fn change(&mut self) {
        let next = match self.next_size() {
            None => Ok(None),
            Some(data) => match self.spares.iter().position(|s| s.hdr.data == data) {
                Some(i) => Ok(Some(self.spares.remove(i))),
                None => {
                    let pending = match self.pending.take() {
                        Some(p) => p,
                        None => Pending::start(self.prep(data)),
                    };
                    pending.take().unwrap_or_else(|| pending.wait()).map(Some)
                }
            },
        };
        let reused = match next {
            Ok(Some(seg)) => SegTier::new(seg, 0, 0, HEADER_BYTES),
            Ok(None) => {
                let i = self.segs.iter().position(|t| t.seg.hdr.data == self.sizes.full).expect("two full segments");
                self.segs.remove(i)
            }
            Err(e) => {
                crate::lx_warn!("scrollback.segment.not_made", &format!("{e}"), dir = self.dir.display());
                // the hot segment itself when it is the only one
                if self.segs.len() == 1 { self.retire_hot() } else { self.segs.remove(0) }
            }
        };
        self.start_use(reused);
        self.evict();
    }

    /// The hot segment stops being written: it keeps the lines it has.
    fn seal_hot(&mut self) {
        let (lines, end) = (self.hot_lines, self.hot_pos);
        let hot = self.segs.last_mut().expect("a hot segment");
        hot.lines = lines;
        hot.used_end = end;
    }

    /// The hot segment, taken out of the history to be used again.
    fn retire_hot(&mut self) -> SegTier {
        self.segs.pop().expect("a hot segment")
    }

    /// Start a new use of `t` as the hot segment, from the next line on.
    fn start_use(&mut self, mut t: SegTier) {
        if self.segs.last().is_some() {
            self.seal_hot();
        }
        let serial = self.next_serial;
        self.next_serial += 1;
        t.seg.hdr = t.seg.hdr.used(serial, 0);
        t.first_line = self.total_lines;
        t.lines = 0;
        t.used_end = HEADER_BYTES;
        let (file, hdr) = (Arc::clone(&t.seg.file), t.seg.hdr);
        self.bin.borrow_mut().cut(move |files| {
            let _ = std::os::unix::fs::FileExt::write_all_at(&*file, &hdr.encode(), 0);
            files.main = Target::at(Arc::clone(&file), HEADER_BYTES);
            files.side = Some(Target::at(file, hdr.index_at()));
        });
        #[cfg(test)]
        if let Some(stall) = self.hooks.stall_after_change.take() {
            self.bin.borrow_mut().cut(move |_| {
                let _ = stall.recv();
            });
        }
        self.segs.push(t);
        self.hot_pos = HEADER_BYTES;
        self.hot_lines = 0;
        self.hot_first_line = self.total_lines;
        self.epoch = serial;
        self.set_marks();
    }

    /// Once two full segments are in use, let go of everything older
    /// than the older of them, and of spares no change will take.
    fn evict(&mut self) {
        let full = self.sizes.full;
        let Some(keep_from) = self.segs.iter().position(|t| t.seg.hdr.data == full) else { return };
        if self.segs.iter().filter(|t| t.seg.hdr.data == full).count() < 2 {
            return;
        }
        let gone: Vec<SegTier> = self.segs.drain(..keep_from).collect();
        let legacy: Vec<legacy::Legacy> = std::mem::take(&mut self.legacy);
        let wanted = self.next_size();
        let (keep, spare_gone): (Vec<Seg>, Vec<Seg>) =
            std::mem::take(&mut self.spares).into_iter().partition(|s| Some(s.hdr.data) == wanted);
        self.spares = keep;
        let mut paths: Vec<std::path::PathBuf> = gone.iter().map(|t| t.seg.path.clone()).collect();
        paths.extend(spare_gone.iter().map(|s| s.path.clone()));
        for l in &legacy {
            paths.extend(l.paths());
        }
        if paths.is_empty() {
            return;
        }
        #[cfg(test)]
        if let Some(n) = self.hooks.evict_upto {
            // the process dies part way through
            for p in paths.iter().take(n) {
                let _ = std::fs::remove_file(p);
            }
            std::mem::forget((gone, legacy, spare_gone));
            return;
        }
        segment::delete_behind(paths, (gone, legacy, spare_gone));
    }

    /// Cheap idempotent flush: a cold-read path calls this before
    /// reading the hot segment, so any line that aged out of the RAM
    /// ring but still sits in the writer's buffer is in the file.
    pub(super) fn ensure_flushed(&self) {
        if !self.has_unflushed.get() {
            return;
        }
        self.bin.borrow_mut().flush();
        self.has_unflushed.set(false);
    }

    /// Call before L3 `execv`: Drop does not run, and lines whose index
    /// stayed in a buffer are in no file the next image can find them
    /// in.  Nothing else is unfinished: no file is being renamed.
    pub fn flush_for_handoff(&self) {
        self.bin.borrow_mut().flush();
        self.has_unflushed.set(false);
    }

    /// End of a burst of output: hand what is buffered to the writer
    /// thread, without waiting for it.
    ///
    /// Without this a pane that printed a few hundred lines and went
    /// quiet would keep them in memory until a buffer filled -- for a
    /// kill to take.
    pub fn hand_off(&self) {
        self.bin.borrow_mut().hand_off();
    }

    /// CSI 3 J: the user's explicit "wipe my history".  It reaches the
    /// files, or `clear` and a restart resurrect everything.  The
    /// bytelog is not touched: replaying it re-applies this very 3J.
    ///
    /// No file is truncated or deleted: the older segments get a header
    /// that says they are unused, and are kept for the next changes; the
    /// hot one starts a new use.  The files of the format before
    /// segments are deleted.
    pub fn clear(&mut self) {
        self.ring.clear();
        self.bin.borrow_mut().flush();
        self.has_unflushed.set(false);
        #[cfg(test)]
        let upto = self.hooks.clear_upto.unwrap_or(CLEAR_STEPS);
        #[cfg(not(test))]
        let upto = 2;
        for l in std::mem::take(&mut self.legacy) {
            for p in l.paths() {
                let _ = std::fs::remove_file(p);
            }
        }
        let hot = self.retire_hot();
        let older: Vec<SegTier> = std::mem::take(&mut self.segs);
        for t in older {
            let hdr = t.seg.hdr.used(0, 0);
            let _ = std::os::unix::fs::FileExt::write_all_at(&*t.seg.file, &hdr.encode(), 0);
            self.spares.push(Seg { hdr, ..t.seg });
        }
        if upto < 2 {
            // the process dies here
            self.segs.push(hot);
            return;
        }
        self.total_lines = 0;
        self.start_use(hot);
        self.bin.borrow_mut().flush();
        // spares beyond the one the next change takes are deleted
        let wanted = self.next_size();
        let mut kept = false;
        let (keep, gone): (Vec<Seg>, Vec<Seg>) = std::mem::take(&mut self.spares).into_iter().partition(|s| {
            let k = !kept && Some(s.hdr.data) == wanted;
            kept |= k;
            k
        });
        self.spares = keep;
        if !gone.is_empty() {
            segment::delete_behind(gone.iter().map(|s| s.path.clone()).collect(), gone);
        }
        self.set_marks();
    }

    /// Reflow re-cuts every line at a new width: the history starts over
    /// as a clear does, and the caller pushes the re-wrapped lines in.
    /// Without that the files would hold duplicate records at every
    /// width the pane was ever at.
    pub(in crate::scrollback) fn reopen_at_cols(mut self, new_cols: usize) -> Result<Self, usize> {
        self.clear();
        self.cols = new_cols;
        Ok(self)
    }
}

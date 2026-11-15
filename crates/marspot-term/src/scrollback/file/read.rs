//! Reading history back out.
//!
//! A read is served from the RAM ring when it can be and from the
//! files when it cannot; the caller cannot tell which, and a read
//! that finds nothing -- a line the budget has let go -- returns
//! `None` rather than guessing.

use super::*;

impl FileScrollback {
    pub fn is_empty(&self) -> bool {
        self.total_lines == 0
    }

    pub fn len(&self) -> usize {
        self.total_lines as usize
    }

    /// The RAM ring's size, the same convention as `MemoryScrollback`.
    pub fn capacity(&self) -> usize {
        self.ram_capacity
    }

    /// Where the RAM ring starts, in logical lines.
    fn ram_first(&self) -> usize {
        (self.total_lines as usize).saturating_sub(self.ring.len())
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<crate::grid::Cell> {
        if line_idx >= self.total_lines as usize {
            return None;
        }
        let ram_first = self.ram_first();
        if line_idx >= ram_first {
            // past what the line holds is a blank
            return Some(self.ring.with_row(line_idx - ram_first, |row| row.get(col).copied().unwrap_or_default()));
        }
        self.with_body(line_idx as u64, |body| ring::decode_body(body).and_then(|cells| cells.get(col).copied()))
            .flatten()
    }

    pub fn read_line(&self, idx: usize) -> Option<Vec<crate::grid::Cell>> {
        if idx >= self.total_lines as usize {
            return None;
        }
        let ram_first = self.ram_first();
        if idx >= ram_first {
            let mut row = self.ring.with_row(idx - ram_first, |row| row[..row.len().min(self.cols)].to_vec());
            row.resize(self.cols, crate::grid::Cell::default());
            return Some(row);
        }
        self.with_body(idx as u64, ring::decode_body).flatten()
    }

    pub fn wrapped_at(&self, idx: usize) -> bool {
        if idx >= self.total_lines as usize {
            return false;
        }
        let ram_first = self.ram_first();
        if idx >= ram_first {
            return self.ring.wrapped(idx - ram_first);
        }
        self.with_body(idx as u64, |body| body.first().is_some_and(|f| f & REC_WRAPPED != 0)).unwrap_or(false)
    }

    pub fn approx_bytes(&self) -> usize {
        // what is held in process, not what is on disk
        self.ring.bytes()
    }

    /// A view of the history for the search worker: its own handles on
    /// the files, so it reads in parallel with the writer.  A segment
    /// the writer uses again while the search reads it answers "gone"
    /// for its lines from then on (see `FileSnapshot`).
    pub fn snapshot_for_search(&self) -> std::io::Result<FileSnapshot> {
        self.bin.borrow_mut().flush();
        self.has_unflushed.set(false);
        let mut parts = Vec::with_capacity(self.legacy.len() + self.segs.len());
        for l in &self.legacy {
            parts.push(super::super::snapshot::Part::legacy(l.bin.try_clone()?, l.idx.try_clone()?, l.first_line, l.lines));
        }
        for (s, t) in self.segs.iter().enumerate() {
            parts.push(super::super::snapshot::Part::segment(
                std::sync::Arc::clone(&t.seg.file),
                t.seg.hdr,
                t.first_line,
                self.seg_lines(s),
            ));
        }
        Ok(FileSnapshot::from_parts(parts, self.total_lines))
    }
}

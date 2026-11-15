//! A frozen view of the scrollback files for the search worker.
//!
//! Its own handles on the files the live writer writes, so a search
//! reads history without taking a lock on, or a copy of, anything the
//! parser is writing.

use std::os::unix::fs::FileExt;

use super::format::*;
use crate::segment::Header;

/// One file of the history, as it was when the view was taken.
pub(super) enum Part {
    Legacy { bin: std::fs::File, idx: std::fs::File, first_line: u64, lines: u64 },
    Segment { file: std::sync::Arc<std::fs::File>, hdr: Header, first_line: u64, lines: u64 },
}

impl Part {
    pub(super) fn legacy(bin: std::fs::File, idx: std::fs::File, first_line: u64, lines: u64) -> Self {
        Self::Legacy { bin, idx, first_line, lines }
    }

    pub(super) fn segment(file: std::sync::Arc<std::fs::File>, hdr: Header, first_line: u64, lines: u64) -> Self {
        Self::Segment { file, hdr, first_line, lines }
    }

    fn first_line(&self) -> u64 {
        match self {
            Self::Legacy { first_line, .. } | Self::Segment { first_line, .. } => *first_line,
        }
    }
}

/// Frozen-at-snapshot read view of a `FileScrollback`, sized for the
/// off-thread search worker.  `total_lines` is captured at snapshot
/// time — appends that arrive after `snapshot_for_search()` returns
/// are invisible to this view; the live-grid merge covers the most
/// recent rows that haven't yet been pushed to scrollback.
///
/// A segment can be used again by the writer while a search reads it
/// -- the oldest full one, once the newest is full -- and then holds
/// other lines at the same places.  So a line of a segment is read
/// between two reads of the segment's use number: if it is not the one
/// the view was taken under, before or after, the line is gone, as a
/// line the budget let go of is.
pub struct FileSnapshot {
    parts: Vec<Part>,
    total_lines: u64,
    /// LRU-of-1 record cache.  `scrollback_search::SearchIter` calls
    /// `wrapped(row)` + `line(row)` and `is_cc_hard_wrap()` (two more
    /// `line()` reads) back-to-back inside `next_logical` — without
    /// this cache that's up to 4 pread+decode round-trips per
    /// physical row.
    last_read: std::cell::RefCell<Option<(u64, std::sync::Arc<(Vec<crate::grid::Cell>, bool)>)>>,
}

// Owned by a single worker thread; the RefCell only ever sees that
// one thread.  Send (not Sync) is what the worker needs.
unsafe impl Send for FileSnapshot {}

impl FileSnapshot {
    pub(super) fn from_parts(parts: Vec<Part>, total_lines: u64) -> Self {
        Self { parts, total_lines, last_read: std::cell::RefCell::new(None) }
    }

    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    fn part_of(&self, idx: u64) -> Option<&Part> {
        self.parts.iter().rev().find(|p| idx >= p.first_line())
    }

    /// Whether the segment of line `idx` is still in the use the view was
    /// taken under.
    fn still_held(&self, idx: u64) -> bool {
        match self.part_of(idx) {
            Some(Part::Segment { file, hdr, .. }) => {
                let mut b = [0u8; 8];
                file.read_exact_at(&mut b, 32).is_ok() && u64::from_le_bytes(b) == hdr.serial
            }
            Some(Part::Legacy { .. }) => true,
            None => false,
        }
    }

    /// The body of logical line `idx`, or `None` when it is not held.
    fn body(&self, idx: u64) -> Option<Vec<u8>> {
        let part = self.part_of(idx)?;
        match part {
            Part::Legacy { bin, idx: index, first_line, lines } => {
                let local = idx - first_line;
                if local >= *lines {
                    return None;
                }
                read_body_at(bin, read_idx_at(index, local).ok()?).ok()
            }
            Part::Segment { file, hdr, first_line, lines } => {
                let local = idx - first_line;
                if local >= *lines {
                    return None;
                }
                let same_use = || self.still_held(idx);
                if !same_use() {
                    return None;
                }
                let mut e = [0u8; 8];
                file.read_exact_at(&mut e, hdr.index_at() + local * 8).ok()?;
                let e = u64::from_le_bytes(e);
                if (e >> 32) as u32 != hdr.serial as u32 {
                    return None;
                }
                let off = e & 0xffff_ffff;
                let body = read_body_at(file, off).ok()?;
                (off + 4 + body.len() as u64 <= hdr.index_at() && same_use()).then_some(body)
            }
        }
    }

    fn read_row(&self, idx: u64) -> Option<std::sync::Arc<(Vec<crate::grid::Cell>, bool)>> {
        if idx >= self.total_lines {
            return None;
        }
        if let Some((cached_idx, cached)) = &*self.last_read.borrow()
            && *cached_idx == idx
        {
            // the segment may have been used again since
            return self.still_held(idx).then(|| std::sync::Arc::clone(cached));
        }
        let body = self.body(idx)?;
        let (head, cells) = body.split_first_chunk::<3>()?;
        let cells = decode_record_cells(head[0], cells, u16::from_le_bytes([head[1], head[2]]) as usize)?;
        let arc = std::sync::Arc::new((cells, head[0] & REC_WRAPPED != 0));
        *self.last_read.borrow_mut() = Some((idx, std::sync::Arc::clone(&arc)));
        Some(arc)
    }

    /// `SearchSource::line` for the search engine.
    pub fn search_line(&self, idx: u64) -> Option<Vec<crate::grid::Cell>> {
        self.read_row(idx).map(|a| a.0.clone())
    }

    /// `SearchSource::wrapped` for the search engine.
    pub fn search_wrapped(&self, idx: u64) -> bool {
        self.read_row(idx).map(|a| a.1).unwrap_or(false)
    }
}

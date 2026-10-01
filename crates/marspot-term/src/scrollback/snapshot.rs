//! A frozen view of the scrollback files for the search worker.
//!
//! Its own fds against the same paths the live writer appends to, so
//! a search reads history without taking a lock on, or a copy of,
//! anything the parser is writing.

use super::format::*;

/// B3 — frozen-at-snapshot read view of a `FileScrollback`, sized for
/// the off-thread search worker.  Owns its own read-only fds so the
/// worker can `pread()` the file in parallel with the live writer
/// (POSIX guarantees pread on one fd is safe against concurrent
/// O_APPEND on another).  `total_lines` is captured at snapshot time
/// — appends that arrive after `snapshot_for_search()` returns are
/// invisible to this view; the live-grid merge in B4 covers the
/// most-recent rows that haven't yet been pushed to scrollback.
///
/// Cheap to construct: 2 file opens + 1 flush of the live BufWriters.
/// No clone of any in-RAM buffer — the worker pays a pread per row,
/// served entirely by the kernel page cache for recently-written
/// pages and by disk for older ones.
pub struct FileSnapshot {
    bin: std::fs::File,
    idx: std::fs::File,
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
    /// Built by `FileScrollback::snapshot_for_search` from fds it
    /// opened against its own paths.  The cache starts empty.
    pub(super) fn from_fds(bin: std::fs::File, idx: std::fs::File, total_lines: u64) -> Self {
        Self {
            bin,
            idx,
            total_lines,
            last_read: std::cell::RefCell::new(None),
        }
    }

    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    fn read_row(&self, idx: u64) -> Option<std::sync::Arc<(Vec<crate::grid::Cell>, bool)>> {
        if idx >= self.total_lines {
            return None;
        }
        if let Some((cached_idx, cached)) = &*self.last_read.borrow()
            && *cached_idx == idx
        {
            return Some(std::sync::Arc::clone(cached));
        }
        let off = read_idx_at(&self.idx, idx).ok()?;
        let (cells, wrapped) = read_record_at(&self.bin, off).ok()?;
        let arc = std::sync::Arc::new((cells, wrapped));
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

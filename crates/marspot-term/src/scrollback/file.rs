//! The file-backed scrollback: a session's history on disk.
//!
//! Split out of `scrollback.rs` because this is the half that owns
//! files, and files outlive the process that wrote them. Everything
//! here has to survive a crash, a rollback to an older binary, and a
//! reflow that re-cuts every line.
//!
//! The history is a handful of segments (`crate::segment`):
//! `<stem>.seg.<slot>` beside the path it was opened with, each a
//! 64-byte header, a record area and, at the end, an index of eight
//! bytes a line -- the record's offset in the low half, the low half of
//! the segment's use number in the high half.  The records are those of
//! format v4 (see `format.rs`), plus notes: a command mark or the text
//! of a line's clusters, written right after the line's record (see
//! `notes.rs`).
//!
//! **The depth is a budget, not a mechanism**: two full segments of 128
//! MiB of records each, the depth the hot and cold pair had.  A new pane
//! starts with a segment of 1 MiB and each next one is twice as large
//! until the full size; when the second full one comes into use the
//! smaller ones are deleted, and from then on the older full one is
//! used again in place whenever the newer is full.  Making a segment
//! ready -- created, given its blocks, sized -- is done on a thread
//! nobody waits for (`crate::async_writer::behind`), asked for well
//! before it is needed.  The steady state makes no system call that
//! creates, renames, deletes, grows or truncates a file.
//!
//! Files of format v2 to v4 (`scrollback.bin` / `.idx`, the cold pair
//! and their sidecars) are read where they are, as the oldest history,
//! and deleted once the budget passes them (`legacy.rs`).

use super::format::*;
use super::snapshot::FileSnapshot;
use crate::segment::{HEADER_BYTES, Seg};

pub struct FileScrollback {
    /// The directory and stem of the segments.
    dir: std::path::PathBuf,
    stem: String,
    cols: usize,
    ram_capacity: usize,
    /// The writer of the hot segment: records and notes at `hot_pos`,
    /// index entries alongside.  In a RefCell so cold reads (via
    /// `&self`-only cell_at) can flush it before reading the file: a
    /// line that aged out of the RAM ring may still be in its buffer.
    bin: std::cell::RefCell<crate::async_writer::AsyncWriter>,
    /// Set by every push, cleared by the flush of a cold read.
    has_unflushed: std::cell::Cell<bool>,
    /// The newest `ram_capacity` lines, read without touching the file.
    ring: ring::Ring,
    /// Lines ever pushed since the files were opened, including those
    /// the budget has since let go: a line keeps its number.
    total_lines: u64,
    /// Reused scratch buffer for record serialisation; zero alloc
    /// after the first push sizes it.
    scratch: Vec<u8>,
    /// Room for a row numbered by position, renumbered by value.
    canon: Vec<crate::grid::PackedCell>,
    /// Files of the format before segments, oldest first, read-only.
    legacy: Vec<legacy::Legacy>,
    /// Segments in use, oldest first; the last is the hot one.
    segs: Vec<tiers::SegTier>,
    /// Where the next record goes in the hot segment, and how many lines
    /// it holds.
    hot_pos: u64,
    hot_lines: u64,
    /// The hot segment's first line.
    hot_first_line: u64,
    /// Past either of these the next push stops to ask for the next
    /// segment, or to change to it: two comparisons a line.
    mark: u64,
    line_mark: u64,
    /// The use number the next segment change gives.
    next_serial: u64,
    /// The slot the next segment made is given.  Never one a file still
    /// being deleted behind may have.
    next_slot: u32,
    /// Segment sizes.
    sizes: Sizes,
    /// Segments made ready and not used, kept for the next change.
    spares: Vec<Seg>,
    /// The next segment, being made ready.
    pending: Option<std::sync::Arc<crate::segment::Pending>>,
    /// Which run of line numbering the hot segment's lines belong to:
    /// its use number.  Changes with every segment change, clear and
    /// reflow.
    pub(super) epoch: u64,
    #[cfg(test)]
    pub(super) hooks: Hooks,
}

// Raw mmap ptrs are private to this struct and the kernel takes care
// of cross-thread coherence — `FileScrollback` itself is owned by a
// single L3 process so there's no inter-process concurrent mutation
// either.
unsafe impl Send for FileScrollback {}

/// Record bytes of the first and of a full segment; each has an index
/// an eighth of that.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sizes {
    pub(super) first: u64,
    pub(super) full: u64,
}

impl Sizes {
    /// 1 MiB growing to 128 MiB, or what `MARSPOT_SCROLLBACK_HOT_CAP_MB`
    /// says a full segment is (0 is the smallest there is, for tests
    /// that want every few lines to change segments).
    pub(super) fn from_env() -> Self {
        let full = hot_bytes_cap().max(4096);
        Self { first: (full / 8).clamp(1024, 1 << 20), full }
    }

    pub(super) fn index_for(data: u64) -> u64 {
        (data / 8).max(64) / 8 * 8
    }
}

/// What a test stops or holds up.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Hooks {
    /// Steps of making a segment ready to do (`segment::STEPS`).
    pub(super) prep_upto: Option<usize>,
    /// Held by the preparation until the test lets it go.
    pub(super) prep_gate: Option<std::sync::mpsc::Receiver<()>>,
    /// The record bytes the next preparation asks for, in place of the
    /// size it should have.
    pub(super) prep_bytes: Option<u64>,
    /// Files of the budget's eviction to delete before stopping.
    pub(super) evict_upto: Option<usize>,
    /// Steps of a clear to do (`write::CLEAR_STEPS`).
    pub(super) clear_upto: Option<usize>,
    /// Held by the writer thread right after it has written the header
    /// of the next segment change, until the test lets it go.
    pub(super) stall_after_change: Option<std::sync::mpsc::Receiver<()>>,
}

/// How much of the records is handed to the writer thread at once, and
/// how many such buffers may be queued or being written.  The index
/// entries of the records in a buffer travel with it, so the two areas
/// lose the same lines to a kill, and with the buffer being filled that
/// is at most `BIN_BUF_BYTES * (BIN_DEPTH + 1)` of records.
const BIN_BUF_BYTES: usize = 64 * 1024;
const BIN_DEPTH: usize = 3;

mod legacy;
mod notes;
mod open;
mod read;
mod ring;
mod tiers;
mod write;

impl Drop for FileScrollback {
    fn drop(&mut self) {
        // Everything buffered reaches the page cache before the files
        // close.  No fsync.  The mappings go with the tiers.
        self.bin.borrow_mut().flush();
        // a segment still being made would be created, sized or given
        // its header after the files were let go: a reopen of the same
        // history could list it half made and delete it, or miss it and
        // give its own next segment the same slot
        if let Some(pending) = self.pending.take() {
            drop(pending.wait());
        }
    }
}

#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod plan_tests_late;
#[cfg(test)]
mod tests;

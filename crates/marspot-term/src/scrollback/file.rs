//! The file-backed scrollback: a session's history on disk.
//!
//! Split out of `scrollback.rs` because this is the half that owns
//! files, and files outlive the process that wrote them. Everything
//! here has to survive a crash, a rollback to an older binary, and a
//! reflow that re-cuts every line.

use super::format::*;
use super::snapshot::FileSnapshot;

// ─── File-backed scrollback (A1) ──────────────────────────────────
//
// Per-session persistent file pair living at
// `paths::scrollback_bin_path(id) / scrollback_idx_path(id)`.  Format
// is documented byte-for-byte below.
//
// Invariants:
//   - `.bin` starts with a 32-byte header (magic / version / cell_abi
//     / created_ns).  cell_abi must equal CELL_BYTES so a future
//     Cell-encoding bump rejects mis-paired binaries cleanly.
//   - `.idx` is a dense `[u64 LE byte_offset]` array PLUS a sentinel
//     pointing at .bin EOF.  `len(idx) = N + 1` where N = lines in
//     `.bin`.  `entry_0 = 32` always.
//   - Most-recent `ram_capacity` lines live in a flat-Vec RAM ring
//     (mirrors `MemoryScrollback`'s zero-alloc shape).  Reads of these
//     never touch the file.  Reads of older lines `pread` directly.
//   - Writer-side has a 64 KiB BufWriter; reader-side uses a separate
//     fd opened read-only so reads never see partially-flushed bytes
//     from the writer's buffer.  Recent lines are ALWAYS in the RAM
//     ring, so any cell_at that misses the ring is for a line already
//     fully written to disk — no consistency hazard.
//   - No fsync.  Drop flushes the writer; a crash truncates the
//     trailing partial record on next open (rec_len + 4 > file_len).

pub struct FileScrollback {
    bin_path: std::path::PathBuf,
    idx_path: std::path::PathBuf,
    cols: usize,
    ram_capacity: usize,
    // A2: `bin` / `idx` writers wrapped in RefCell so cold reads
    // (via `&self`-only cell_at) can flush BufWriters before reading
    // the file via mmap/pread.  Without this, a line that aged out
    // of the RAM ring but is still in BufWriter's user-space buffer
    // would not be visible to mmap, breaking the "ring miss ⇒ file
    // hit" invariant.
    bin: std::cell::RefCell<crate::async_writer::AsyncWriter>,
    /// Buffered, like `bin`.  It was a raw `File` until 2026-08-19,
    /// on the reasoning that 8 B per push is one syscall (~5 µs),
    /// "well within budget".  The estimate was right and the budget
    /// was measured on the wrong workload: interactive output pushes
    /// a few lines a second, a `cat` of a large file pushes 200 000,
    /// and a profile of the shipped path put **63 % of the parse
    /// thread's samples** in this write.
    ///
    /// What the raw file bought was a narrower crash window for the
    /// index — but the asymmetry it created was the odd part: `bin`
    /// has always been a `BufWriter`, so an unclean kill already
    /// loses its buffered tail, and `open()` already reconciles the
    /// two by scanning and truncating.  Buffering both makes them
    /// lose the same tail instead of different ones, and the flush
    /// order (bin, then idx) keeps the index from ever pointing past
    /// the data it indexes.
    idx: std::cell::RefCell<crate::async_writer::AsyncWriter>,
    /// Separate read-only fd for cold reads.  Reads of recent lines
    /// hit the RAM ring, so missing-from-file isn't observable.
    bin_for_read: std::fs::File,
    idx_for_read: std::fs::File,
    // A2: mmap state for cold reads.  Pointer is NULL until first
    // cold read forces an mmap.  Remap happens when a needed offset
    // exceeds the current mapping length (file has grown since the
    // last mmap).  Both fields wrapped in `Cell` so `cell_at(&self)`
    // can update them on remap without taking `&mut self`.
    bin_mmap_ptr: std::cell::Cell<*mut u8>,
    bin_mmap_len: std::cell::Cell<usize>,
    idx_mmap_ptr: std::cell::Cell<*mut u8>,
    idx_mmap_len: std::cell::Cell<usize>,
    /// Set true on every `push_line`, false after a successful
    /// flush in the cold-read path.  Cheap "is the file consistent
    /// for a cold read?" probe so we don't pay a syscall when the
    /// user is just reading from the RAM ring.
    has_unflushed: std::cell::Cell<bool>,
    // RAM ring: zero-alloc flat-Vec mirror of the newest `ram_capacity`
    // lines, with parallel wrapped flags.
    ram_cells: Vec<crate::grid::Cell>,
    /// How many of each slot's `cols` cells were actually written.
    ///
    /// The ring stores fixed-width rows, but the rows themselves are
    /// not: a 122-column grid showing emoji prose fills ~47 of them.
    /// Padding every slot out to `cols` on the way in costs a memset
    /// of the remainder per pushed line — on the parse thread, for a
    /// tier whose whole purpose is to be read *rarely* (it is the
    /// front-line cache; a miss just goes to the file).  Recording the
    /// length instead moves that work to the reader, which is the side
    /// that can afford it.  Stale cells past `ram_lens[slot]` are
    /// whatever the slot held before and must never be handed out.
    ram_lens: Vec<u16>,
    ram_wrapped: Vec<bool>,
    ram_head: usize,
    ram_len: usize,
    /// Total lines ever pushed since creation — no eviction in v1 so
    /// this also equals scrollback length.
    total_lines: u64,
    /// Current `.bin` EOF (matches what's been written including
    /// unflushed BufWriter bytes — used as the next record's offset).
    bin_tail_offset: u64,
    /// Reused scratch buffer for record serialisation; zero alloc
    /// after the first push sizes it.
    scratch: Vec<u8>,

    // F2+5 — hot/cold rotation state.
    /// Path of the cold-tier `.bin` (derived from `bin_path` once at
    /// open time; held so rotate doesn't have to recompute).
    cold_bin_path: std::path::PathBuf,
    /// Path of the cold-tier `.idx`.
    cold_idx_path: std::path::PathBuf,
    /// Read-only fd on the cold `.bin`, opened lazily when cold tier
    /// is present (either from disk at startup or after an in-process
    /// rotation).  None = no cold tier yet, OR cold was unreadable.
    cold_bin_for_read: Option<std::fs::File>,
    /// Read-only fd on the cold `.idx`.
    cold_idx_for_read: Option<std::fs::File>,
    /// First logical line_idx represented in the cold file (= 0 on
    /// the first rotation in this process; bumps on subsequent
    /// rotations because the new cold = previously-hot rows that
    /// already had a non-zero `hot_first_line`).  Across L3 restart
    /// this resets to 0 — line_idx is per-process, not persistent.
    cold_first_line: u64,
    /// Count of rows in the cold file.  cold_first_line + cold_total_lines
    /// always equals hot_first_line (the boundary).
    cold_total_lines: u64,
    /// First logical line_idx whose row lives in the current hot file
    /// (= 0 on fresh session, = total_lines at moment of last rotation).
    /// Used by `cell_at` to translate logical → local hot file idx.
    hot_first_line: u64,
    /// Bytes threshold for hot file before rotation fires.  Read once
    /// from the env at open; doesn't re-check on every push so a mid-
    /// session env change is ignored.
    hot_bytes_cap: u64,
    /// Which run of line numbering this file's local line indices
    /// belong to.  See [`new_epoch`].
    pub(super) epoch: u64,
    /// Command marks for the hot file's lines, keyed by line index
    /// within it (`super::sidecar`).  `None` when the file could not be
    /// opened: marks are a cache, so the pane keeps working without
    /// them and the next open rebuilds.
    pub(super) marks: Option<std::fs::File>,
}

// Raw mmap ptrs are private to this struct and the kernel takes care
// of cross-thread coherence — `FileScrollback` itself is owned by a
// single L3 process so there's no inter-process concurrent mutation
// either.  Marker impls let the type cross thread boundaries when
// embedded in `Scrollback` (which `Send` is naturally derived for).
unsafe impl Send for FileScrollback {}

/// Write-buffer sizes for the two files a `FileScrollback` keeps.
///
/// They are a pair, not two independent numbers.  An unclean kill
/// (SIGKILL, a panic, `execv` without the handoff flush) loses whatever
/// sits in each buffer, and `open()` reconciles the survivors by
/// trusting the index only as far as the data file actually reaches.
/// So what matters is which buffer holds MORE unwritten lines:
///
///   bin  64 KiB / ~100 B per record  ≈ 640 lines
///   idx   4 KiB / 8 B per record     =  512 lines
///
/// Keeping idx's window strictly smaller preserves the invariant the
/// recovery scan was written against — the index never claims lines the
/// data file cannot produce — while still amortising the syscall over
/// hundreds of pushes instead of paying one per line (which, on a bulk
/// `cat`, was 63 % of the parse thread; see the `idx` field comment).
const BIN_BUF_BYTES: usize = 64 * 1024;
const IDX_BUF_BYTES: usize = 4 * 1024;

mod open;
mod read;
mod write;

impl Drop for FileScrollback {
    fn drop(&mut self) {
        use std::io::{Seek, SeekFrom};
        // Flush BufWriters so any buffered bytes hit the page cache
        // before our fds close.  No fsync.  Dense idx — no sentinel.
        self.bin.borrow_mut().flush();
        // Unmap any active mmap regions.
        let bp = self.bin_mmap_ptr.get();
        let bl = self.bin_mmap_len.get();
        if !bp.is_null() && bl > 0 {
            unsafe {
                libc::munmap(bp as *mut libc::c_void, bl);
            }
        }
        let ip = self.idx_mmap_ptr.get();
        let il = self.idx_mmap_len.get();
        if !ip.is_null() && il > 0 {
            unsafe {
                libc::munmap(ip as *mut libc::c_void, il);
            }
        }
        let _ = self.bin_for_read.seek(SeekFrom::Start(0));
        let _ = self.idx_for_read.seek(SeekFrom::Start(0));
    }
}

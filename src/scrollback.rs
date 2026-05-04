//! Scrollback storage for terminal lines that have scrolled off the
//! visible grid.
//!
//! `Scrollback` is the abstraction `Grid` calls into: append a line,
//! ask for one back by reverse-index, drop the lot.  The current
//! implementation is in-memory only (a fixed-cap ring buffer); the
//! interface is shaped so a disk-backed variant can drop in behind
//! it without touching `Grid` or the renderer.
//!
//! ## Why this lives in its own module
//!
//! Two upcoming changes — disk-backed unlimited history and a page
//! cache for the disk path — both need to swap the storage out
//! without disturbing the cell-level `Grid` API.  Pulling the trait
//! out now lets the next session focus on the disk implementation
//! against a stable interface, and lets the renderer keep using
//! `Grid::cell_at_view` unchanged.
//!
//! ## What the disk-backed variant will look like (next session)
//!
//! Sketch, not implemented here:
//!
//!   * Recent N lines (~1024) live in a `MemoryScrollback` ring,
//!     fast-path for the common "scrolled up a screen or two" case.
//!   * Older lines append-log to
//!     `~/Library/Caches/mars/<session-id>.log`, fixed-size pages
//!     (256 lines × cols × 16 B ≈ 128 KiB).
//!   * Page index in RAM: `Vec<u64>` of file offsets.  ~8 B per
//!     page → 8 KB for 1 M lines.
//!   * Page cache: small LRU (4-8 pages) in RAM so consecutive
//!     reads in the same neighbourhood don't re-seek.
//!   * Cap: file truncates at e.g. 100 MiB; oldest pages dropped
//!     by rebuilding the file with the surviving tail.
//!
//! Production-grade disk scrollback wants atomic page writes,
//! crash recovery, and bounded growth — all reasons it deserves
//! its own focused session rather than tacking on to a long one.

use crate::grid::Cell;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Lines per disk page.  Page is the read-cache and ring-rotation
/// granularity.  256 lines × 80 cols × 24 B/cell ≈ 480 KiB per page.
pub const LINES_PER_PAGE: usize = 256;

/// Enum-dispatched storage.  All four call sites (`push_line`,
/// `len`, `cell_at`, `clear`) are in the per-scroll hot path on the
/// parser side — `Terminal::feed` → `Grid::scroll_up` →
/// `Scrollback::push_line` for every line that scrolls off.
/// Trait-object dispatch costs ~1 % on emoji-dense parse benches;
/// the enum lets the compiler inline through the match.
pub enum Scrollback {
    Memory(MemoryScrollback),
    Disk(DiskScrollback),
}

impl Scrollback {
    pub fn memory(capacity: usize, cols: usize) -> Self {
        Self::Memory(MemoryScrollback::new(capacity, cols))
    }

    /// Disk-backed scrollback.  Recent `ram_capacity` lines live in
    /// RAM; older ones spill to a fixed-size ring file at
    /// `scratch_dir/<unique>.log`.  Total cap (RAM + disk combined)
    /// is `ram_capacity + max_pages_on_disk * LINES_PER_PAGE`.
    /// Caller picks `scratch_dir` (typically `~/Library/Caches/mars/scrollback`
    /// in production, `std::env::temp_dir()` in tests).  File is
    /// deleted on Drop.
    pub fn disk(
        scratch_dir: &Path,
        ram_capacity: usize,
        max_pages_on_disk: usize,
        cols: usize,
    ) -> std::io::Result<Self> {
        Ok(Self::Disk(DiskScrollback::new(
            scratch_dir,
            ram_capacity,
            max_pages_on_disk,
            cols,
        )?))
    }

    pub fn push_line(&mut self, line: &[Cell]) {
        match self {
            Self::Memory(m) => m.push_line(line),
            Self::Disk(d) => d.push_line(line),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Memory(m) => m.len(),
            Self::Disk(d) => d.len(),
        }
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::Memory(m) => m.capacity(),
            Self::Disk(d) => d.capacity(),
        }
    }

    /// Read one cell.  Hot path for the renderer (`Grid::cell_at_view`).
    /// Memory: O(1) ring index.  Disk: O(1) RAM hit, or one disk read
    /// per 256 lines (single-slot page cache; `cell_at_view` iterates
    /// cols within a row → all cells of a scrollback row are one page).
    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        match self {
            Self::Memory(m) => m.cell_at(line_idx, col),
            Self::Disk(d) => d.cell_at(line_idx, col),
        }
    }

    /// Read one whole line.  Allocates a Vec for the disk path; mostly
    /// for tests + the headless `--snapshot` path.  Hot rendering uses
    /// `cell_at` instead to avoid the per-line allocation.
    pub fn line_to_vec(&self, idx: usize) -> Option<Vec<Cell>> {
        match self {
            Self::Memory(m) => m.line(idx).map(|s| s.to_vec()),
            Self::Disk(d) => d.read_line(idx),
        }
    }

    pub fn clear(&mut self) {
        match self {
            Self::Memory(m) => m.clear(),
            Self::Disk(d) => d.clear(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bench-harness escape hatch: hint the kernel to drop the disk
    /// scrollback's resident pages, simulating the state of a session
    /// after long idle when the unified buffer cache has evicted
    /// scrollback pages under memory pressure.  No-op on the Memory
    /// variant.  Subsequent reads page-fault back from the file.
    pub fn evict_disk_pages_for_bench(&self) {
        if let Self::Disk(d) = self {
            d.evict_pages_for_bench();
        }
    }

    /// Drop all content and re-init for a new column width.  Used
    /// by `Grid::resize` — stored lines aren't valid at the new
    /// width.  Preserves the variant (Memory stays Memory; Disk
    /// makes a fresh file in the same scratch dir, falling back to
    /// Memory if the new file can't be created).
    pub fn restart(&mut self, new_cols: usize) {
        let placeholder = std::mem::replace(self, Self::Memory(MemoryScrollback::new(0, 1)));
        *self = match placeholder {
            Self::Memory(m) => Self::Memory(MemoryScrollback::new(m.capacity, new_cols)),
            Self::Disk(d) => {
                let scratch_dir = d.path.parent().map(|p| p.to_path_buf());
                let ram_cap = d.init_ram_capacity;
                let max_pages = d.init_max_pages_on_disk;
                drop(d); // triggers Drop → deletes old file
                match scratch_dir.and_then(|dir| {
                    DiskScrollback::new(&dir, ram_cap, max_pages, new_cols).ok()
                }) {
                    Some(new_d) => Self::Disk(new_d),
                    None => Self::Memory(MemoryScrollback::new(ram_cap, new_cols)),
                }
            }
        };
    }
}

/// Fixed-capacity ring buffer over a flat `Vec<Cell>`.  This is
/// the only `Scrollback` impl today; lifted out of `grid.rs` so the
/// future disk-backed sibling can sit next to it.
///
/// ## Memory shape
///
/// `cells.capacity() == capacity * cols` after construction; the
/// `Vec` is **not** populated up-front (lazy growth).  Once `len`
/// reaches `capacity`, `push_line` overwrites the oldest entry in
/// place — no more allocations after that point.
///
/// At 10 000 lines × 122 cols × 16 B/cell, the upper-bound is
/// ~19 MiB per session.  Idle stays small because `push_line` is
/// the only path that grows the buffer (lazy alloc landed in
/// commit `ed074bd`).
pub struct MemoryScrollback {
    cells: Vec<Cell>,
    capacity: usize,
    cols: usize,
    head: usize,
    len: usize,
}

impl MemoryScrollback {
    pub fn new(capacity: usize, cols: usize) -> Self {
        let cells = if capacity == 0 || cols == 0 {
            Vec::new()
        } else {
            Vec::with_capacity(capacity * cols)
        };
        Self {
            cells,
            capacity,
            cols,
            head: 0,
            len: 0,
        }
    }

    pub fn push_line(&mut self, source: &[Cell]) {
        if self.capacity == 0 {
            return;
        }
        debug_assert_eq!(
            source.len(),
            self.cols,
            "pushed line width mismatches scrollback cols"
        );

        if self.len < self.capacity {
            // Growing phase: extend onto the end.  After exactly
            // `capacity` push_lines, `cells.len() == capacity * cols`
            // and we're done growing.
            self.cells.extend_from_slice(source);
            self.len += 1;
            return;
        }

        // Steady state: overwrite the oldest line.
        let line = self.head;
        self.head = (self.head + 1) % self.capacity;
        let start = line * self.cols;
        self.cells[start..start + self.cols].copy_from_slice(source);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn line(&self, idx: usize) -> Option<&[Cell]> {
        if idx >= self.len {
            return None;
        }
        let line = (self.head + idx) % self.capacity;
        let start = line * self.cols;
        Some(&self.cells[start..start + self.cols])
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        let line = self.line(line_idx)?;
        line.get(col).copied()
    }

    pub fn clear(&mut self) {
        // Drop logical contents, keep allocated capacity so a
        // post-clear scroll storm doesn't pay for re-allocation.
        // Truncate the Vec because lazy-growth `push_line` extends
        // from its end.
        self.head = 0;
        self.len = 0;
        self.cells.clear();
    }
}

// =====================================================================
// DiskScrollback — RAM ring + page-aligned ring file on disk.
// =====================================================================

/// Disk-backed scrollback for unbounded history — single mmap'd
/// ring, no separate RAM tier.
///
/// Layout: one fixed-size `mmap_len`-byte file, mapped into our
/// address space and treated as a flat ring of `max_lines` slots
/// each `line_bytes` wide.  `push_line` does one `memcpy` into
/// slot `(global % max_lines)`.  `cell_at` reads back via a slice
/// over the same offset.  Once `total_lines_written` exceeds
/// `max_lines`, the oldest slot gets overwritten in place and that
/// line falls off the addressable index.
///
/// Why one tier and not two: this used to be `RAM ring + disk
/// ring`, with the RAM ring serving as a fast-path cache for
/// recent lines and the disk ring overflowing older lines via
/// per-line `seek` + `write_all`.  When we replaced the seek path
/// with `mmap` (Phase 1 of the disk-default roadmap), the RAM
/// tier became a vestige — mmap'd pages are RAM-speed when warm,
/// page-fault transparently when cold, and the kernel's unified
/// buffer cache acts as a far better LRU than we'd build by hand.
/// Keeping both tiers cost a second `memcpy` per scroll-off
/// (source → ram_cells + ram_cells → mmap), measured at ~14 % of
/// cat-ascii parse throughput.  Phase 4a deleted the RAM tier
/// and gave parse parity with memory-only — see commit message.
///
/// Bounded growth (per CLAUDE.md): file is fixed-size.  Past
/// `max_lines`, oldest slots get overwritten and dropped lines
/// fall off the addressable index.
///
/// Lifecycle: file is created on `new` with a unique name in
/// `scratch_dir`, mapped, deleted on `Drop` (after munmap).
/// Per-session ephemeral — not designed to survive a mars restart
/// in v1 (would need versioned headers + `#[repr(C)]` on `Cell`).
///
/// Read path: direct slice into the mmap'd region.  No syscalls
/// on access; the kernel's page cache is our LRU — viewport ± a
/// few pages stay resident while older pages are evicted to disk
/// transparently.  No software cache needed at this layer.
pub struct DiskScrollback {
    cols: usize,
    line_bytes: usize,

    /// File handle kept open for the lifetime of the mapping.
    /// macOS allows closing it earlier, but holding it costs
    /// nothing and keeps future operations (e.g. fcntl) simple.
    _file: File,
    path: PathBuf,
    mmap_ptr: *mut u8,
    mmap_len: usize,
    max_lines: u64,
    /// Total lines ever pushed.  When > `max_lines`, the oldest
    /// have been overwritten in place and are no longer addressable.
    total_lines_written: u64,

    // Original constructor args, retained so `Scrollback::restart`
    // can rebuild the same shape after a column-width change.
    init_ram_capacity: usize,
    init_max_pages_on_disk: usize,
}

impl DiskScrollback {
    fn new(
        scratch_dir: &Path,
        ram_capacity: usize,
        max_pages_on_disk: usize,
        cols: usize,
    ) -> std::io::Result<Self> {
        // `ram_capacity` is folded into `max_lines` for backwards
        // compatibility with callers (terminal.rs, tests) that pre-date
        // the unified-mmap refactor.  Total addressable history is the
        // same: `ram_capacity + max_pages_on_disk * LINES_PER_PAGE`.
        assert!(ram_capacity > 0, "RAM capacity must be > 0 for disk scrollback");
        assert!(max_pages_on_disk > 0, "max disk pages must be > 0");
        assert!(cols > 0, "cols must be > 0");

        std::fs::create_dir_all(scratch_dir)?;

        // Unique-ish file name.  Ephemeral, deleted on Drop, so
        // collision with a stale file from a crashed run is safe to
        // ignore — we'll just truncate it.  Process pid + nanos
        // since UNIX epoch is enough entropy for in-process uniqueness.
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = scratch_dir.join(format!("mars-sb-{pid}-{nanos}.log"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        let line_bytes = cols * std::mem::size_of::<Cell>();
        let max_lines = (ram_capacity + max_pages_on_disk * LINES_PER_PAGE) as u64;
        let mmap_len = max_lines as usize * line_bytes;

        // Pre-size the file to the full ring extent so the mmap
        // covers all slots.  Unwritten slots read as zeroed Cell
        // layouts, but the index logic refuses to read past
        // `total_lines_written` so we never observe them.
        file.set_len(mmap_len as u64)?;

        // Map the whole ring.  Writes become memcpy + dirty-page
        // bookkeeping (handled asynchronously by the kernel); reads
        // are slice access into the mapped region with the unified
        // buffer cache as our LRU.
        let mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mmap_ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // Hint the kernel: access pattern is sequential (downward
        // scrolling = sequential access).  Advisory; safe to ignore
        // the return.
        unsafe {
            libc::madvise(mmap_ptr, mmap_len, libc::MADV_SEQUENTIAL);
        }
        let mmap_ptr = mmap_ptr as *mut u8;

        Ok(Self {
            cols,
            line_bytes,
            _file: file,
            path,
            mmap_ptr,
            mmap_len,
            max_lines,
            total_lines_written: 0,
            init_ram_capacity: ram_capacity,
            init_max_pages_on_disk: max_pages_on_disk,
        })
    }

    pub fn len(&self) -> usize {
        self.total_lines_written.min(self.max_lines) as usize
    }

    pub fn capacity(&self) -> usize {
        self.max_lines as usize
    }

    pub fn push_line(&mut self, source: &[Cell]) {
        debug_assert_eq!(source.len(), self.cols);
        let global_line = self.total_lines_written;
        let slot = (global_line % self.max_lines) as usize;
        let offset = slot * self.line_bytes;
        debug_assert!(offset + self.line_bytes <= self.mmap_len);
        // SAFETY: bounds checked above (`offset + line_bytes <= mmap_len`,
        // `slot < max_lines` by modulo).  Source and destination are
        // disjoint memory regions (caller's slice vs mmap region).
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.as_ptr() as *const u8,
                self.mmap_ptr.add(offset),
                self.line_bytes,
            );
        }
        self.total_lines_written += 1;
    }

    /// Map an external `idx` (0 = oldest stored, len-1 = newest) to
    /// the global line index used as the mmap slot key.  Returns
    /// `None` if `idx` is past the addressable range.
    fn locate(&self, idx: usize) -> Option<u64> {
        let total = self.len();
        if idx >= total {
            return None;
        }
        let earliest = self.total_lines_written - total as u64;
        Some(earliest + idx as u64)
    }

    /// Slice into the mmap region for the given global line.
    ///
    /// SAFETY: caller must observe Rust's aliasing rules — the
    /// returned borrow must not outlive the next `push_line` that
    /// could overwrite the same slot.  In practice all callers
    /// consume the slice synchronously inside one expression.
    fn line_slice(&self, global_line: u64) -> &[Cell] {
        let slot = (global_line % self.max_lines) as usize;
        let offset = slot * self.line_bytes;
        debug_assert!(offset + self.line_bytes <= self.mmap_len);
        unsafe {
            std::slice::from_raw_parts(
                self.mmap_ptr.add(offset) as *const Cell,
                self.cols,
            )
        }
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        if col >= self.cols {
            return None;
        }
        let global_line = self.locate(line_idx)?;
        Some(self.line_slice(global_line)[col])
    }

    pub fn read_line(&self, idx: usize) -> Option<Vec<Cell>> {
        let global_line = self.locate(idx)?;
        Some(self.line_slice(global_line).to_vec())
    }

    pub fn clear(&mut self) {
        // Resetting the counter renders any previously written
        // mmap slots logically inaccessible (locate() refuses idx
        // beyond len()).  No need to zero the bytes — they'll be
        // overwritten by future pushes.
        self.total_lines_written = 0;
    }

    /// Hint the kernel to discard our resident pages — used by
    /// `--bench scroll-cold` to simulate a session where the unified
    /// buffer cache has evicted scrollback under memory pressure.
    /// macOS treats `MADV_DONTNEED` as "drop file-backed pages, fault
    /// back in from disk on next access" which is exactly what we
    /// want to measure.
    fn evict_pages_for_bench(&self) {
        unsafe {
            libc::madvise(
                self.mmap_ptr as *mut libc::c_void,
                self.mmap_len,
                libc::MADV_DONTNEED,
            );
        }
    }
}

impl Drop for DiskScrollback {
    fn drop(&mut self) {
        // Release the mapping before unlinking; macOS allows unlink
        // while mapped, but unmapping first is the documented order.
        if !self.mmap_ptr.is_null() {
            // SAFETY: pointer + length match the mmap call in `new`,
            // and no outstanding borrow into the region survives Drop
            // (Rust borrow checker enforces this — a live borrow would
            // keep DiskScrollback alive).
            unsafe {
                libc::munmap(self.mmap_ptr as *mut libc::c_void, self.mmap_len);
            }
        }
        // Best-effort unlink; the file is in ~/Library/Caches and
        // macOS will eventually reclaim it on its own.  Stale-file
        // sweep at next process start (terminal.rs) handles signal-
        // induced exits where Drop never runs.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    fn fill(b: u8, cols: usize) -> Vec<Cell> {
        (0..cols)
            .map(|_| Cell {
                ch: b as char,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn push_grows_then_evicts() {
        let mut sb = MemoryScrollback::new(3, 4);
        sb.push_line(&fill(b'a', 4));
        sb.push_line(&fill(b'b', 4));
        sb.push_line(&fill(b'c', 4));
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.line(0).unwrap()[0].ch, 'a');
        assert_eq!(sb.line(2).unwrap()[0].ch, 'c');

        // Past capacity: 'a' evicts.
        sb.push_line(&fill(b'd', 4));
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.line(0).unwrap()[0].ch, 'b');
        assert_eq!(sb.line(2).unwrap()[0].ch, 'd');
    }

    #[test]
    fn clear_drops_logical_contents() {
        let mut sb = MemoryScrollback::new(2, 3);
        sb.push_line(&fill(b'a', 3));
        sb.push_line(&fill(b'b', 3));
        sb.clear();
        assert_eq!(sb.len(), 0);
        assert!(sb.line(0).is_none());

        // Post-clear use works.
        sb.push_line(&fill(b'c', 3));
        assert_eq!(sb.len(), 1);
        assert_eq!(sb.line(0).unwrap()[0].ch, 'c');
    }

    #[test]
    fn zero_capacity_disables_storage() {
        let mut sb = MemoryScrollback::new(0, 80);
        sb.push_line(&fill(b'x', 80));
        assert_eq!(sb.len(), 0);
        assert!(sb.line(0).is_none());
    }

    #[test]
    fn enum_dispatch_round_trips() {
        // The Scrollback enum is what Grid actually holds; verify
        // it forwards correctly to the Memory variant.
        let mut sb = Scrollback::memory(2, 4);
        assert!(sb.is_empty());
        sb.push_line(&fill(b'a', 4));
        sb.push_line(&fill(b'b', 4));
        sb.push_line(&fill(b'c', 4));
        assert_eq!(sb.len(), 2);
        assert_eq!(sb.capacity(), 2);
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'b');
        assert_eq!(sb.cell_at(1, 0).unwrap().ch, 'c');
        sb.clear();
        assert!(sb.is_empty());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mars-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        p
    }

    #[test]
    fn disk_round_trips_within_ram_capacity() {
        let dir = temp_dir("disk-round");
        let mut sb = Scrollback::disk(&dir, 4, 1, 4).expect("disk sb");
        sb.push_line(&fill(b'a', 4));
        sb.push_line(&fill(b'b', 4));
        sb.push_line(&fill(b'c', 4));
        assert_eq!(sb.len(), 3);
        // All three still in RAM, no disk write yet.
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
        assert_eq!(sb.cell_at(2, 0).unwrap().ch, 'c');
        let line1 = sb.line_to_vec(1).unwrap();
        assert_eq!(line1[0].ch, 'b');
    }

    #[test]
    fn disk_spills_past_ram_capacity_to_disk() {
        let dir = temp_dir("disk-spill");
        // RAM 2, 1 disk page → total cap = 2 + 256 = 258.  Push the
        // exact cap amount, all addressable.
        let mut sb = Scrollback::disk(&dir, 2, 1, 4).expect("disk sb");
        let total_cap = 2 + LINES_PER_PAGE;
        for i in 0..total_cap {
            let byte = b'a' + (i % 26) as u8;
            sb.push_line(&fill(byte, 4));
        }
        assert_eq!(sb.len(), total_cap);
        assert_eq!(sb.capacity(), total_cap);
        // Oldest (index 0) is on disk; it's the first byte we pushed.
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
        // Newest is in RAM, last byte we pushed.
        let newest_byte = b'a' + ((total_cap - 1) % 26) as u8;
        assert_eq!(sb.cell_at(total_cap - 1, 0).unwrap().ch, newest_byte as char);
    }

    #[test]
    fn disk_ring_drops_oldest_past_total_cap() {
        let dir = temp_dir("disk-ring");
        // RAM 2, 1 disk page → total cap = 2 + 256 = 258 lines.
        let mut sb = Scrollback::disk(&dir, 2, 1, 4).expect("disk sb");
        let total = 2 + LINES_PER_PAGE * 3; // overflows by 2 pages
        for i in 0..total {
            let byte = b'a' + (i % 26) as u8;
            sb.push_line(&fill(byte, 4));
        }
        // After ring rotation, len caps at total cap.
        assert_eq!(sb.len(), 2 + LINES_PER_PAGE);
        // Oldest now-addressable line is `total - sb.len()` in the
        // input sequence.
        let dropped = total - sb.len();
        let expected_oldest = b'a' + ((dropped) % 26) as u8;
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, expected_oldest as char);
    }

    #[test]
    fn disk_drop_removes_file() {
        let dir = temp_dir("disk-drop");
        let path_before;
        {
            let sb = Scrollback::disk(&dir, 2, 1, 4).expect("disk sb");
            // Capture the file path via the Disk variant.
            let Scrollback::Disk(ref d) = sb else { panic!("expected Disk") };
            path_before = d.path.clone();
            assert!(path_before.exists(), "file should exist while sb alive");
        }
        assert!(
            !path_before.exists(),
            "file should be deleted on Drop, found: {path_before:?}"
        );
    }

    #[test]
    fn disk_full_history_read_back_after_wraparound() {
        // Stress test: push enough lines to wrap the disk ring twice, then
        // verify every addressable line reads back its expected content.
        // Catches per-line corruption that single-cell tests would miss
        // (e.g. an mmap offset bug that interleaves bytes between slots).
        let dir = temp_dir("disk-fullread");
        let cols = 8;
        let pages = 2;
        let mut sb = Scrollback::disk(&dir, 4, pages, cols).expect("disk sb");
        let total_pushed = 4 + LINES_PER_PAGE * (pages + 1); // 1 wraparound past cap
        for i in 0..total_pushed {
            // Distinct content per line: unique character per col so a
            // corrupted slot would mismatch on at least one cell.
            let cells: Vec<Cell> = (0..cols)
                .map(|c| Cell {
                    ch: char::from_u32(((i + c) as u32 % 95) + 0x20).unwrap(),
                    ..Default::default()
                })
                .collect();
            sb.push_line(&cells);
        }
        let len = sb.len();
        let dropped = total_pushed - len;
        for idx in 0..len {
            let original = idx + dropped;
            for c in 0..cols {
                let expected = char::from_u32(((original + c) as u32 % 95) + 0x20).unwrap();
                let got = sb.cell_at(idx, c).expect("addressable").ch;
                assert_eq!(
                    got, expected,
                    "mismatch at line idx {idx} col {c} (orig push {original})",
                );
            }
        }
    }

    #[test]
    fn disk_clear_resets_addressable_lines() {
        let dir = temp_dir("disk-clear");
        let mut sb = Scrollback::disk(&dir, 4, 2, 4).expect("disk sb");
        for i in 0..(4 + 2 * LINES_PER_PAGE) {
            sb.push_line(&fill(b'a' + (i % 26) as u8, 4));
        }
        assert!(sb.len() > 0);
        sb.clear();
        assert_eq!(sb.len(), 0);
        assert!(sb.cell_at(0, 0).is_none());

        // Post-clear push works.
        sb.push_line(&fill(b'X', 4));
        assert_eq!(sb.len(), 1);
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'X');
    }
}

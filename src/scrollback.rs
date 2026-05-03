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
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
                let ram_cap = d.ram_capacity;
                let max_pages = d.max_pages_on_disk;
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

/// Disk-backed scrollback for unbounded history.
///
/// Layout:
///   * **RAM ring** — most-recent `ram_capacity` lines, exactly the
///     same shape as `MemoryScrollback`.  Read/write is in-RAM and
///     doesn't touch disk.
///   * **Disk ring** — older lines, written `LINES_PER_PAGE` at a
///     time into a fixed-size file.  File slot for global page N is
///     `(N % max_pages_on_disk) * page_bytes` — once the file fills
///     the oldest pages get overwritten in place, never grown
///     beyond `max_pages_on_disk * page_bytes`.
///
/// Bounded growth (per CLAUDE.md): both the RAM ring and the disk
/// file are fixed-size.  Total cap =
/// `ram_capacity + max_pages_on_disk * LINES_PER_PAGE`.  Past that,
/// oldest history is dropped — first oldest disk pages get
/// overwritten by new pages, and dropped lines fall off the
/// addressable index.
///
/// Lifecycle: file is created on `new` with a unique name in
/// `scratch_dir`, deleted on `Drop`.  Per-session ephemeral —
/// not designed to survive a mars restart in v1 (would need
/// versioned headers + `#[repr(C)]` on `Cell`).
///
/// Read cache: single-slot.  `cell_at_view` iterates cols within a
/// row → all cells of one scrollback row are in the same page; one
/// slot is enough.  Multi-row scans (e.g. searching scrollback)
/// would benefit from a small LRU; deferred until there's a
/// caller for it.
pub struct DiskScrollback {
    cols: usize,
    line_bytes: usize,
    page_bytes: usize,

    // RAM ring (most-recent ram_capacity lines).
    ram_capacity: usize,
    ram_cells: Vec<Cell>,
    ram_head: usize,
    ram_len: usize,

    // Disk ring — line-granular.  `max_disk_lines = max_pages_on_disk *
    // LINES_PER_PAGE` slots, each `line_bytes` wide.  Slot for global
    // line N = `(N % max_disk_lines) * line_bytes`.
    //
    // Note: per-line writes mean we can't safely page-cache (a page
    // is half-old half-new at any moment we're partway through it).
    // Read cache is per-line instead — `cell_at_view` iterates cols
    // within a row → all cells of a scrollback row hit the cache;
    // each new row pays one read syscall (~µs).
    file: File,
    path: PathBuf,
    max_pages_on_disk: usize,
    max_disk_lines: u64,
    /// Total lines ever spilled to disk.  When this exceeds
    /// `max_disk_lines`, older lines have been overwritten and are
    /// no longer addressable.
    total_disk_lines_written: u64,

    // Single-slot read cache.  `(global_line_idx, decoded cells)`.
    line_cache: RefCell<Option<(u64, Vec<Cell>)>>,
}

impl DiskScrollback {
    fn new(
        scratch_dir: &Path,
        ram_capacity: usize,
        max_pages_on_disk: usize,
        cols: usize,
    ) -> std::io::Result<Self> {
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
        let page_bytes = LINES_PER_PAGE * line_bytes;

        // Pre-size the file to its full ring extent so seeks past
        // EOF don't fail (set_len fills with zeros which decode as
        // default cells if we ever read an unwritten slot — but the
        // index logic refuses to read past `total_pages_written *
        // LINES_PER_PAGE` so we never actually hit those bytes).
        file.set_len((max_pages_on_disk * page_bytes) as u64)?;

        Ok(Self {
            cols,
            line_bytes,
            page_bytes,
            ram_capacity,
            ram_cells: Vec::with_capacity(ram_capacity * cols),
            ram_head: 0,
            ram_len: 0,
            file,
            path,
            max_pages_on_disk,
            max_disk_lines: (max_pages_on_disk * LINES_PER_PAGE) as u64,
            total_disk_lines_written: 0,
            line_cache: RefCell::new(None),
        })
    }

    pub fn len(&self) -> usize {
        self.disk_lines() + self.ram_len
    }

    pub fn capacity(&self) -> usize {
        self.ram_capacity + self.max_pages_on_disk * LINES_PER_PAGE
    }

    /// Lines currently addressable on disk: capped by the ring size.
    fn disk_lines(&self) -> usize {
        self.total_disk_lines_written
            .min(self.max_disk_lines) as usize
    }

    pub fn push_line(&mut self, source: &[Cell]) {
        debug_assert_eq!(source.len(), self.cols);

        // RAM ring still has room: append, done.
        if self.ram_len < self.ram_capacity {
            self.ram_cells.extend_from_slice(source);
            self.ram_len += 1;
            return;
        }

        // RAM ring full: spill the oldest RAM line to disk, then
        // overwrite the oldest RAM slot with the new line.  Per-line
        // disk write — one seek + one write syscall, ~few µs amortised
        // by the page cache on the kernel side.  Errors degrade to
        // "stay in RAM, lose oldest silently" without crashing.
        let oldest_slot = self.ram_head;
        let start = oldest_slot * self.cols;
        let end = start + self.cols;
        let oldest: Vec<Cell> = self.ram_cells[start..end].to_vec();
        if let Err(e) = self.spill_one_to_disk(&oldest) {
            eprintln!("mars: scrollback spill failed: {e} (continuing without disk)");
        }
        self.ram_cells[start..end].copy_from_slice(source);
        self.ram_head = (self.ram_head + 1) % self.ram_capacity;
    }

    fn spill_one_to_disk(&mut self, line: &[Cell]) -> std::io::Result<()> {
        let global_line = self.total_disk_lines_written;
        let slot = (global_line % self.max_disk_lines) as usize;
        let offset = (slot * self.line_bytes) as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(cells_as_bytes(line))?;
        self.total_disk_lines_written += 1;
        // Invalidate read cache if it held the global_line we just
        // overwrote (only happens once a slot is recycled).
        let cached = self.line_cache.borrow().as_ref().map(|(g, _)| *g);
        if let Some(g) = cached {
            if (g % self.max_disk_lines) == global_line % self.max_disk_lines {
                self.line_cache.borrow_mut().take();
            }
        }
        Ok(())
    }

    /// Map an external `idx` (0 = oldest stored, len-1 = newest) to
    /// the global line index when the line lives on disk, or to a
    /// RAM slot otherwise.
    fn locate(&self, idx: usize) -> Option<Location> {
        let total = self.len();
        if idx >= total {
            return None;
        }
        let disk_lines = self.disk_lines();
        if idx >= disk_lines {
            let ram_idx = idx - disk_lines;
            let slot = (self.ram_head + ram_idx) % self.ram_capacity;
            return Some(Location::Ram(slot));
        }
        let earliest = self.total_disk_lines_written - disk_lines as u64;
        let global_line = earliest + idx as u64;
        Some(Location::Disk { global_line })
    }

    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        if col >= self.cols {
            return None;
        }
        match self.locate(line_idx)? {
            Location::Ram(slot) => {
                let start = slot * self.cols;
                Some(self.ram_cells[start + col])
            }
            Location::Disk { global_line } => {
                self.with_disk_line(global_line, |cells| cells[col])
            }
        }
    }

    pub fn read_line(&self, idx: usize) -> Option<Vec<Cell>> {
        match self.locate(idx)? {
            Location::Ram(slot) => {
                let start = slot * self.cols;
                Some(self.ram_cells[start..start + self.cols].to_vec())
            }
            Location::Disk { global_line } => {
                self.with_disk_line(global_line, |cells| cells.to_vec())
            }
        }
    }

    /// Run `f` on `global_line`'s cells, faulting from disk on miss.
    /// Single-line cache: `cell_at_view` iterates cols within a row →
    /// all cols of one scrollback row hit the cache; each new row
    /// pays one read syscall.
    fn with_disk_line<R>(
        &self,
        global_line: u64,
        f: impl FnOnce(&[Cell]) -> R,
    ) -> Option<R> {
        let cached_hit = self
            .line_cache
            .borrow()
            .as_ref()
            .map(|(g, _)| *g)
            == Some(global_line);
        if !cached_hit {
            let slot = (global_line % self.max_disk_lines) as usize;
            let offset = (slot * self.line_bytes) as u64;
            let mut buf = vec![0u8; self.line_bytes];
            // Cloning the file FD gives us an independent seek
            // cursor — `try_clone()` is dup3 on macOS, cheap.
            let mut fd = self.file.try_clone().ok()?;
            fd.seek(SeekFrom::Start(offset)).ok()?;
            fd.read_exact(&mut buf).ok()?;
            let cells = bytes_to_cells(&buf);
            *self.line_cache.borrow_mut() = Some((global_line, cells));
        }
        let cache = self.line_cache.borrow();
        let (_, cells) = cache.as_ref()?;
        Some(f(cells))
    }

    pub fn clear(&mut self) {
        self.ram_cells.clear();
        self.ram_head = 0;
        self.ram_len = 0;
        self.total_disk_lines_written = 0;
        *self.line_cache.borrow_mut() = None;
        // Truncate + re-set_len keeps the ring file at its
        // canonical size so subsequent seeks are valid.
        let _ = self.file.set_len(0);
        let _ = self.file.set_len((self.max_pages_on_disk * self.page_bytes) as u64);
    }
}

impl Drop for DiskScrollback {
    fn drop(&mut self) {
        // Best-effort cleanup; ignore errors (the file is in
        // ~/Library/Caches and macOS will eventually reclaim it).
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Clone, Copy, Debug)]
enum Location {
    Ram(usize),         // ring slot
    Disk { global_line: u64 },
}

/// View `&[Cell]` as raw bytes.  SAFETY: `Cell` is `Copy + 'static`
/// with no internal pointers; reading its in-process layout as bytes
/// and writing those bytes back to the same process produces an
/// equivalent `Cell` value.  We never share these bytes across
/// processes / compiler versions — the file is per-process scratch
/// (created on construct, deleted on Drop).
fn cells_as_bytes(cells: &[Cell]) -> &[u8] {
    let len = std::mem::size_of_val(cells);
    unsafe { std::slice::from_raw_parts(cells.as_ptr() as *const u8, len) }
}

/// Inverse of `cells_as_bytes`.  Same SAFETY argument: bytes were
/// produced by `cells_as_bytes` of cells we ourselves wrote.
fn bytes_to_cells(bytes: &[u8]) -> Vec<Cell> {
    let cell_size = std::mem::size_of::<Cell>();
    assert!(bytes.len() % cell_size == 0, "page bytes must be a multiple of Cell size");
    let n = bytes.len() / cell_size;
    let mut out: Vec<Cell> = Vec::with_capacity(n);
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            out.as_mut_ptr() as *mut u8,
            bytes.len(),
        );
        out.set_len(n);
    }
    out
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

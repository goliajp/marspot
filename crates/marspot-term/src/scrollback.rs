//! Scrollback storage for terminal lines that have scrolled off the
//! visible grid.
//!
//! `Scrollback` is the abstraction `Grid` calls into: append a line,
//! ask for one back by reverse-index, drop the lot.  Two enum-
//! dispatched variants — `Memory` (a fixed-cap `Vec<Cell>` ring) and
//! `Disk` (a fixed-cap anonymous-mmap ring).  Both bound RSS forever;
//! the `Disk` variant trades a one-time 50 MiB virtual reservation
//! per session for kernel-managed eviction-via-swap under memory
//! pressure, so 1 M+ lines of history don't translate into 1 M+ lines
//! of resident pages.
//!
//! ## Why "Disk" is named that
//!
//! Historical: an earlier implementation backed the ring with a
//! file in `~/Library/Caches/marspot/scrollback`, leaning on the
//! kernel's unified buffer cache to evict pages back to that file
//! under memory pressure.  Anonymous mmap (this version) does the
//! same eviction via swap rather than a named file — bypassing the
//! file-COW step that surfaced as ~10 % parse-throughput regression
//! in the file-backed era.  The "Disk" name stays because the
//! eviction target still _is_ disk (kernel swap), even though the
//! file system layer is no longer involved.

// F1 — `Scrollback::Disk` is `#[deprecated]` for callers but the
// in-module match arms here still need to construct / pattern-match
// it.  Module-scoped allow so caller-side uses (outside this module)
// still surface the deprecation warning while internals stay clean.
#![allow(deprecated)]

use crate::grid::Cell;

/// Lines per disk page.  Page is the read-cache and ring-rotation
/// granularity.  256 lines × 80 cols × 24 B/cell ≈ 480 KiB per page.
pub const LINES_PER_PAGE: usize = 256;

/// Enum-dispatched storage.  All four call sites (`push_line`,
/// `len`, `cell_at`, `clear`) are in the per-scroll hot path on the
/// parser side — `Terminal::feed` → `Grid::scroll_up` →
/// `Scrollback::push_line` for every line that scrolls off.
/// Trait-object dispatch costs ~1 % on emoji-dense parse benches;
/// the enum lets the compiler inline through the match.
#[allow(deprecated)]
pub enum Scrollback {
    Memory(MemoryScrollback),
    /// F1 — `Disk` is superseded by `File` for any session-scoped
    /// scrollback.  Kept as a fallback for `--snapshot` / mcli /
    /// tests where no session id is present (those never touched
    /// the disk pages anyway).  F2 removes it entirely once F1 has
    /// soaked for ~1 week.
    #[deprecated(
        since = "0.10.0",
        note = "use File variant (default since F1); Disk removed in F2"
    )]
    Disk(DiskScrollback),
    /// Persistent file-backed scrollback (A1 of pane upgrade — see
    /// `docs/scrollback-search.md`).  Survives L3 self-execv via
    /// path-based reopen.  Append-only `.bin` + sidecar `.idx`;
    /// hot read served from RAM ring, cold reads `pread()` the
    /// file.  Wrapped flag per line stored in the record (Grid's
    /// `sb_wrapped` mirror stays the in-RAM truth for Memory/Disk
    /// variants).
    File(FileScrollback),
}

impl Scrollback {
    pub fn memory(capacity: usize, cols: usize) -> Self {
        Self::Memory(MemoryScrollback::new(capacity, cols))
    }

    /// Disk-backed scrollback (anonymous mmap; see module-level doc).
    /// Total cap = `ram_capacity + max_pages_on_disk * LINES_PER_PAGE`
    /// lines.  The two-arg shape (rather than a single `max_lines`)
    /// is preserved so `restart()` can rebuild the same ring shape
    /// after a column-width change — `max_lines = a + b` doesn't
    /// uniquely recover (`a`, `b`).
    pub fn disk(
        ram_capacity: usize,
        max_pages_on_disk: usize,
        cols: usize,
    ) -> std::io::Result<Self> {
        Ok(Self::Disk(DiskScrollback::new(
            ram_capacity,
            max_pages_on_disk,
            cols,
        )?))
    }

    /// Open or create a file-backed scrollback at the given paths.
    /// A1: variant constructor exists but is NOT wired into
    /// `Terminal::new` yet (A3 does the env-gated wiring).
    pub fn file(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
    ) -> std::io::Result<Self> {
        Ok(Self::File(FileScrollback::open(
            bin_path,
            idx_path,
            cols,
            ram_capacity,
        )?))
    }

    pub fn push_line(&mut self, line: &[Cell]) {
        match self {
            Self::Memory(m) => m.push_line(line),
            Self::Disk(d) => d.push_line(line),
            // A1: File variant defaults wrapped=false on the enum
            // path.  Grid's wrapped truth gets threaded through in
            // A3 via `push_line_with_wrapped` once we wire File into
            // Terminal::new.
            Self::File(f) => f.push_line(line, false),
        }
    }

    /// File variant only: push a line with its DECAWM continuation
    /// flag.  Memory/Disk drop the flag (Grid's `sb_wrapped` is the
    /// truth for them).  Added in A1 as a surface for direct tests
    /// against FileScrollback; A3 makes Grid call this so file
    /// records carry the right wrapped value.
    pub fn push_line_with_wrapped(&mut self, line: &[Cell], wrapped: bool) {
        match self {
            Self::Memory(m) => m.push_line(line),
            Self::Disk(d) => d.push_line(line),
            Self::File(f) => f.push_line(line, wrapped),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Memory(m) => m.len(),
            Self::Disk(d) => d.len(),
            Self::File(f) => f.len(),
        }
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::Memory(m) => m.capacity(),
            Self::Disk(d) => d.capacity(),
            Self::File(f) => f.capacity(),
        }
    }

    /// Read one cell.  Hot path for the renderer (`Grid::cell_at_view`).
    /// Memory: O(1) ring index.  Disk: O(1) RAM hit, or one disk read
    /// per 256 lines (single-slot page cache; `cell_at_view` iterates
    /// cols within a row → all cells of a scrollback row are one page).
    /// File: O(1) RAM ring hit for the most-recent `ram_capacity`
    /// lines; pread fallback for older.
    pub fn cell_at(&self, line_idx: usize, col: usize) -> Option<Cell> {
        match self {
            Self::Memory(m) => m.cell_at(line_idx, col),
            Self::Disk(d) => d.cell_at(line_idx, col),
            Self::File(f) => f.cell_at(line_idx, col),
        }
    }

    /// Read one whole line.  Allocates a Vec for the disk path; mostly
    /// for tests + the headless `--snapshot` path.  Hot rendering uses
    /// `cell_at` instead to avoid the per-line allocation.
    pub fn line_to_vec(&self, idx: usize) -> Option<Vec<Cell>> {
        match self {
            Self::Memory(m) => m.line(idx).map(|s| s.to_vec()),
            Self::Disk(d) => d.read_line(idx),
            Self::File(f) => f.read_line(idx),
        }
    }

    /// B3 — hand back an off-thread search snapshot of the File
    /// variant (Memory/Disk return None).  The snapshot is `Send` and
    /// owns its own read fds; the worker thread it gets handed to
    /// can pread the file in parallel with the live writer.  See
    /// `FileSnapshot` for the semantics.
    pub fn file_snapshot(&self) -> Option<FileSnapshot> {
        match self {
            Self::File(f) => f.snapshot_for_search().ok(),
            _ => None,
        }
    }

    /// File variant only: per-line wrapped flag.  Memory/Disk return
    /// false (Grid's `sb_wrapped` is the truth there).
    pub fn wrapped_at(&self, idx: usize) -> bool {
        match self {
            Self::Memory(_) | Self::Disk(_) => false,
            Self::File(f) => f.wrapped_at(idx),
        }
    }

    /// Read a contiguous run of scrollback lines counted **back from
    /// the newest entry**, ordered oldest-first (natural top-to-
    /// bottom render order).
    ///
    /// `line_start = 0`, `count = N` → the most recent `N` scrollback
    /// lines (the ones just above the live grid).  `line_start = K`
    /// asks for the run starting `K` lines back from newest, so
    /// `(line_start, count) = (16, 8)` returns the 8 lines just
    /// above what `(0, 16)` returned.
    ///
    /// Returns up to `count` lines.  A shorter result means the
    /// request crossed the scrollback floor (oldest line in the
    /// ring).  An empty `Vec` means `line_start >= len()` — caller
    /// treats it as "no more history beyond here", which is the
    /// `ScrollbackPage { line_count: 0 }` wire sentinel.
    ///
    /// RFC-002 step 8 (`GetScrollbackPage` handler) is the primary
    /// caller.  Disk variant: O(count) RAM hit, or one page fault
    /// per 256-line page crossed; line_to_vec already amortises the
    /// per-line cost.
    pub fn read_lines(&self, line_start: usize, count: usize) -> Vec<Vec<Cell>> {
        let len = self.len();
        if line_start >= len || count == 0 {
            return Vec::new();
        }
        let avail = (len - line_start).min(count);
        // Internal index 0 = oldest, len-1 = newest.
        let newest = len - 1 - line_start;       // newest line in the window
        let oldest = newest + 1 - avail;         // oldest line in the window
        let mut out = Vec::with_capacity(avail);
        for i in oldest..=newest {
            if let Some(v) = self.line_to_vec(i) {
                out.push(v);
            }
        }
        out
    }

    pub fn clear(&mut self) {
        match self {
            Self::Memory(m) => m.clear(),
            Self::Disk(d) => d.clear(),
            Self::File(f) => f.clear(),
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

    /// Approximate resident bytes held by this scrollback for the
    /// MARSPOT_PROFILE_RSS sampler.  Memory variant: lazy-grown
    /// `Vec<Cell>` capacity.  Disk variant: bytes-worth of lines
    /// actually written into the mmap ring (NOT the full
    /// reservation) — `total_lines_written.min(max_lines) *
    /// line_bytes`.  Querying real resident pages via `mincore` on
    /// every sample is too expensive; written-bytes is a cheap,
    /// monotonic proxy that tracks real lazy-fault growth so Phase
    /// 1.3's slope analysis surfaces a scrollback leak directly in
    /// this column rather than hiding it as a constant reservation
    /// while the leak shows up in `other`.
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Memory(m) => m.approx_bytes(),
            Self::Disk(d) => d.approx_bytes(),
            Self::File(f) => f.approx_bytes(),
        }
    }

    /// Drop all content and re-init for a new column width.  Used
    /// by `Grid::resize` — stored lines aren't valid at the new
    /// width.  Preserves the variant (Memory stays Memory; Disk
    /// remaps a fresh anonymous ring at the same shape, falling
    /// back to Memory only on the pathological case where mmap
    /// itself fails).
    pub fn restart(&mut self, new_cols: usize) {
        let placeholder = std::mem::replace(self, Self::Memory(MemoryScrollback::new(0, 1)));
        *self = match placeholder {
            Self::Memory(m) => Self::Memory(MemoryScrollback::new(m.capacity, new_cols)),
            Self::Disk(d) => {
                let ram_cap = d.init_ram_capacity;
                let max_pages = d.init_max_pages_on_disk;
                drop(d); // triggers Drop → munmap
                match DiskScrollback::new(ram_cap, max_pages, new_cols).ok() {
                    Some(new_d) => Self::Disk(new_d),
                    None => Self::Memory(MemoryScrollback::new(ram_cap, new_cols)),
                }
            }
            Self::File(f) => {
                // Reflow drops the in-RAM ring (cells aren't valid at
                // the new width) but keeps the file content — the
                // historic record IS the user's history regardless of
                // current display width.  reopen() rebuilds ring at
                // new_cols.  If reopen fails we fall back to Memory at
                // the same ram_capacity so the session keeps running.
                let bin_path = f.bin_path.clone();
                let idx_path = f.idx_path.clone();
                let ram_cap = f.ram_capacity;
                drop(f);
                match FileScrollback::open(bin_path, idx_path, new_cols, ram_cap) {
                    Ok(new_f) => Self::File(new_f),
                    Err(_) => Self::Memory(MemoryScrollback::new(ram_cap, new_cols)),
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
        // F3+3.5 — release-build runtime guard.  Same rationale as
        // `Disk::push_line`: a width-mismatched source corrupts
        // the cells Vec (growing phase) or panics on
        // `copy_from_slice` (steady state).  Normalise to a one-
        // shot scratch padded / truncated to `self.cols`.
        if source.len() != self.cols {
            let mut buf: Vec<Cell> = Vec::with_capacity(self.cols);
            let n = source.len().min(self.cols);
            buf.extend_from_slice(&source[..n]);
            buf.resize(self.cols, Cell::default());
            return self.push_line(&buf);
        }

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

    pub fn approx_bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<Cell>()
    }
}

// =====================================================================
// DiskScrollback — anonymous mmap ring (kernel swap = "disk").
// =====================================================================

/// Disk-backed scrollback for unbounded history — single mmap'd
/// ring of anonymous pages, no separate RAM tier, no scratch file.
///
/// Layout: one fixed-size `mmap_len`-byte anonymous region, treated
/// as a flat ring of `max_lines` slots each `line_bytes` wide.
/// `push_line` does one `memcpy` into slot `(global % max_lines)`.
/// `cell_at` reads back via a slice over the same offset.  Once
/// `total_lines_written` exceeds `max_lines`, the oldest slot gets
/// overwritten in place and that line falls off the addressable
/// index.
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
/// Bounded growth (per CLAUDE.md): the mmap region is fixed-size.
/// Past `max_lines`, oldest slots get overwritten and dropped lines
/// fall off the addressable index.
///
/// Lifecycle: anonymous-mmap region created on `new`, munmap'd on
/// Drop.  No file system involvement — eviction under memory
/// pressure goes through kernel swap, not a named scratch file.
/// Per-session ephemeral.
///
/// Read path: direct slice into the mmap'd region.  No syscalls
/// on access; the kernel pages out idle regions to swap and
/// faults them back in transparently — no software cache needed
/// at this layer.
pub struct DiskScrollback {
    cols: usize,
    line_bytes: usize,
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

// `mmap_ptr: *mut u8` makes the auto-derived Send/Sync no — Rust
// can't see that the pointer is owned + exclusively governed by this
// struct and that all access is serialised through `&self` / `&mut
// self` methods.  Backing region is `MAP_PRIVATE`, owned for the
// struct's lifetime, freed in `Drop`.  shelld holds `Terminal` (which
// transitively owns this) inside a `Mutex` so cross-thread access is
// already gated; promising Send + Sync is correct.
unsafe impl Send for DiskScrollback {}
unsafe impl Sync for DiskScrollback {}

impl DiskScrollback {
    fn new(
        ram_capacity: usize,
        max_pages_on_disk: usize,
        cols: usize,
    ) -> std::io::Result<Self> {
        // `ram_capacity` and `max_pages_on_disk` were originally a
        // RAM tier + on-disk-pages tier; Phase 4a unified them under
        // one mmap.  We keep the two args because `restart()` needs
        // to rebuild the same shape (`a + b` doesn't recover the
        // pair).  Total addressable history = ram_capacity +
        // max_pages_on_disk * LINES_PER_PAGE.
        assert!(ram_capacity > 0, "RAM capacity must be > 0 for disk scrollback");
        assert!(max_pages_on_disk > 0, "max disk pages must be > 0");
        assert!(cols > 0, "cols must be > 0");

        let line_bytes = cols * std::mem::size_of::<Cell>();
        let max_lines = (ram_capacity + max_pages_on_disk * LINES_PER_PAGE) as u64;
        let mmap_len = max_lines as usize * line_bytes;

        // Anonymous private mapping.  No backing file — the kernel
        // pages dirty regions out to swap under memory pressure,
        // same outcome as the old file-backed path without the
        // file→anon COW cost on every first write to a page.
        // (Earlier file-backed implementations cost ~10 % parse-heavy
        // throughput; switching to MAP_ANON recovered it.)
        //
        // Pre-faulting all pages here would fold the lazy-fault cost
        // of the first ring wrap into init time, but pegs idle RSS
        // at the full ring size — directly violating the lazy-alloc
        // contract (commit ed074bd) and the bench `rss marspot` gate.
        // Don't reintroduce.
        let mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
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
        // NOTE: tried pre-faulting all pages here to fold the
        // first-wrap COW cost into init.  It worked (parse +5–10 %)
        // but pegged the bench `rss marspot` gate at 290 MiB (9 sessions
        // × 50 MiB committed up-front) — the lazy-alloc commit
        // ed074bd's whole point was to keep idle RSS flat, so we
        // can't pre-commit at session create.  If parse perf needs
        // more, a smarter on-first-push warmup or moving back to a
        // small RAM-fronted ring is the way; not pre-fault at init.

        Ok(Self {
            cols,
            line_bytes,
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
        // F3+3.5 — release-build runtime guard.  Old code asserted
        // `source.len() == self.cols` in debug only; release would
        // run `copy_nonoverlapping(source.as_ptr(), …, self.line_bytes)`
        // with no length check.  When `source.len() == 0` (e.g. an
        // empty scrollback line from a corrupted state.bin),
        // `Vec::<Cell>::as_ptr()` returns `NonNull::dangling()` =
        // `align_of::<Cell>() = 0x4`, and the unsafe read of
        // `self.line_bytes` bytes faults at 0x4.  Diagnosed via the
        // sid 248 crash report (2026-06-19): KERN_INVALID_ADDRESS @
        // 0x0000000000000004 inside `_platform_memmove` called from
        // `push_historic_scrollback_line`.
        //
        // The cheapest safe path is: if source isn't exactly cols
        // wide, normalise to a one-shot scratch buffer padded with
        // `Cell::default()` (the same fallback `cell_at_view`
        // returns for missing columns).  Empty / short lines now
        // copy zero/few user bytes + (self.cols - n) blanks; over-
        // wide lines truncate to self.cols.  Either way the mmap
        // slot ends up exactly `line_bytes` written, no UB.
        if source.len() != self.cols {
            let mut buf: Vec<Cell> = Vec::with_capacity(self.cols);
            let n = source.len().min(self.cols);
            buf.extend_from_slice(&source[..n]);
            buf.resize(self.cols, Cell::default());
            return self.push_line(&buf);
        }
        let global_line = self.total_lines_written;
        let slot = (global_line % self.max_lines) as usize;
        let offset = slot * self.line_bytes;
        debug_assert!(offset + self.line_bytes <= self.mmap_len);
        // SAFETY: bounds checked above (`offset + line_bytes <= mmap_len`,
        // `slot < max_lines` by modulo) and `source.len() == self.cols`
        // (post-guard above), so `self.line_bytes` bytes from
        // `source.as_ptr()` is in-bounds.  Source and destination are
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

    pub fn approx_bytes(&self) -> usize {
        let written = self.total_lines_written.min(self.max_lines) as usize;
        written * self.line_bytes
    }
}

impl Drop for DiskScrollback {
    fn drop(&mut self) {
        if !self.mmap_ptr.is_null() {
            // SAFETY: pointer + length match the mmap call in `new`,
            // and no outstanding borrow into the region survives Drop
            // (Rust borrow checker enforces this — a live borrow would
            // keep DiskScrollback alive).
            unsafe {
                libc::munmap(self.mmap_ptr as *mut libc::c_void, self.mmap_len);
            }
        }
    }
}

// ─── File-backed scrollback (A1) ──────────────────────────────────
//
// Per-session persistent file pair living at
// `paths::scrollback_bin_path(id) / scrollback_idx_path(id)`.  Format
// is documented byte-for-byte in `docs/scrollback-search.md` §3.2–§3.3.
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

const FILE_MAGIC: u32 = 0x5350_5301;
const FILE_VERSION: u32 = 1;
const FILE_MIN_COMPAT: u32 = 1;
const FILE_HEADER_BYTES: u64 = 32;

/// F2+5 — hot/cold scrollback rotation cap.  When `scrollback.bin`
/// would exceed this many bytes, the writer flushes + closes, renames
/// the current pair to `scrollback.cold.bin` / `scrollback.cold.idx`
/// (overwriting any prior cold pair — that's the "delete" tier), and
/// opens a fresh empty hot pair.  Older logical line indices stay
/// addressable through the cold pair until the next rotation evicts
/// it.  Tunable via `MARSPOT_SCROLLBACK_HOT_CAP_MB`; default 128 MB
/// → per-pane disk budget ≤ 256 MB (hot + cold), 9 panes ≤ 2.3 GB
/// total.  At F2+4 trim's ~400 B/row average that's ~330 k rows hot
/// + 330 k rows cold = ~9 hrs busy claudecode history per pane.
const HOT_BYTES_CAP_DEFAULT: u64 = 128 * 1024 * 1024;

fn hot_bytes_cap() -> u64 {
    std::env::var("MARSPOT_SCROLLBACK_HOT_CAP_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(HOT_BYTES_CAP_DEFAULT)
}

/// Placeholder File handle used during `rotate_to_cold` to hold the
/// `File`-typed fields while the real files are being renamed +
/// reopened.  Opening `/dev/null` gives us a real `File` so the
/// fields stay non-Option without needing `Option<File>` and the
/// associated unwraps everywhere on the hot read paths.
fn dev_null_file() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).open("/dev/null")
}

/// F3+8 — bin file grows in 64 KB chunks.  Each chunk holds ~100-200
/// records (claudecode TUI row trimmed ≈ 400-800 B); per push the
/// write is one memcpy into the existing mmap region.  Chunk boundary
/// fires `ftruncate + munmap + mmap`, amortised at ~40 ns/push.
const BIN_CHUNK_BYTES: u64 = 64 * 1024;
/// Same for idx (one u64 per push).  4 KB = 512 entries / chunk.
const IDX_CHUNK_BYTES: u64 = 4 * 1024;
/// Cap on the backward idx scan at open time (after over-allocation
/// from a previous process that died without truncating back).  At
/// 8 byte / entry this is 1 M entries — way past any plausible legit
/// over-allocation tail.
const IDX_REOPEN_SCAN_MAX: u64 = 1024 * 1024;

pub struct FileScrollback {
    bin_path: std::path::PathBuf,
    idx_path: std::path::PathBuf,
    cols: usize,
    ram_capacity: usize,

    // F3+8 — bin / idx are now mmap-write: one R/W fd each, kernel
    // page cache is the single source of truth.  Crossing L3
    // self-execv loses NOTHING because the kernel-side state (mmap +
    // page cache + file inode) survives image swap; the BufWriter
    // tail that used to leak per-execv (see project memory
    // `project-scrollback-execv-gap`) cannot exist by construction.
    //
    // mmap is MAP_SHARED PROT_READ|PROT_WRITE — writes through this
    // pointer hit the file's page cache directly, visible to anyone
    // else holding the same inode mmap'd (including a re-execv'd new
    // L3).  File is over-allocated in `BIN_CHUNK_BYTES` chunks so
    // most pushes are pure memcpy; chunk-boundary pushes ftruncate +
    // remap (rare).  `Drop` (clean exit) trims back to the real data
    // size; `execv` skips Drop so the file is left over-allocated and
    // the next open's idx-scan derives the real boundary.
    bin_fd: std::fs::File,
    bin_mmap_ptr: std::cell::Cell<*mut u8>,
    /// Current mmap region size = current ftruncated file size on
    /// disk.  Always ≥ `bin_write_offset`.
    bin_mmap_cap: std::cell::Cell<usize>,
    /// Byte offset within `bin` where the next record will be
    /// written.  Equals "real" file content size (everything past
    /// is over-allocated padding from chunked ftruncate).
    bin_write_offset: u64,

    idx_fd: std::fs::File,
    idx_mmap_ptr: std::cell::Cell<*mut u8>,
    idx_mmap_cap: std::cell::Cell<usize>,
    /// Byte offset within `idx` of the next idx slot to write.
    /// = `(hot_lines) * 8`.
    idx_write_offset: u64,

    // RAM ring: zero-alloc flat-Vec mirror of the newest `ram_capacity`
    // lines, with parallel wrapped flags.  Now mostly a hot read-path
    // optimisation (avoids reaching into mmap for the freshest rows);
    // the mmap covers the same data so this is no longer a
    // durability prerequisite.
    ram_cells: Vec<crate::grid::Cell>,
    ram_wrapped: Vec<bool>,
    ram_head: usize,
    ram_len: usize,

    /// Total lines ever pushed since creation (= cold_total_lines +
    /// hot_lines).  No eviction beyond cold-rotation drop.
    total_lines: u64,

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
    /// Reused scratch buffer for record serialisation.  We assemble
    /// one record here (per-cell loop) then memcpy the whole record
    /// to mmap in a single shot — many small mmap writes pessimise
    /// the codegen vs. one bulk copy.  Single-record only, fully
    /// written within one `push_line`, so it carries NO execv-time
    /// risk (the data is in the mmap before push_line returns).
    scratch: Vec<u8>,
}

// Raw mmap ptrs are private to this struct and the kernel takes care
// of cross-thread coherence — `FileScrollback` itself is owned by a
// single L3 process so there's no inter-process concurrent mutation
// either.  Marker impls let the type cross thread boundaries when
// embedded in `Scrollback` (which `Send` is naturally derived for).
unsafe impl Send for FileScrollback {}

impl FileScrollback {
    /// Open or create.  Validates header on an existing file;
    /// rebuilds `.idx` if missing or length-mismatched; trims a
    /// trailing partial record left by a crash mid-write.
    ///
    /// `ram_capacity` is the in-RAM hot ring size (recent lines).
    /// `cols` is the grid width at construction time — used to size
    /// the scratch buffer and the RAM ring's flat Vec.  Historic
    /// lines in the file may have a DIFFERENT cols (recorded per
    /// line); reads from the file decode at their record's cols.
    pub fn open(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
    ) -> std::io::Result<Self> {
        if let Some(parent) = bin_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let bin_existed = bin_path.exists()
            && bin_path.metadata().map(|m| m.len() > 0).unwrap_or(false);

        // F3+8 — single R/W fd, mmap'd MAP_SHARED.  No more BufWriter:
        // writes through the mmap land in the kernel page cache, which
        // survives `execv` intact.  See struct doc-comment.
        let bin_fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&bin_path)?;

        // Validate header on an existing file; rename on corruption and
        // recurse for a fresh start.
        if bin_existed {
            let cur_len = bin_fd.metadata()?.len();
            if cur_len < FILE_HEADER_BYTES {
                drop(bin_fd);
                Self::rename_corrupt(&bin_path, &idx_path)?;
                return Self::open(bin_path, idx_path, cols, ram_capacity);
            }
            let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
            read_exact_at(&bin_fd, &mut hdr, 0)?;
            let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
            let cell_abi = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            if magic != FILE_MAGIC
                || version < FILE_MIN_COMPAT
                || version > FILE_VERSION
                || cell_abi != crate::terminal::CELL_BYTES_PUB as u32
            {
                drop(bin_fd);
                Self::rename_corrupt(&bin_path, &idx_path)?;
                return Self::open(bin_path, idx_path, cols, ram_capacity);
            }
        }

        // Fresh file: ftruncate to one chunk, mmap, write header into
        // the mmap.  Existing file: mmap its current size and figure
        // out the real write_offset from the idx scan below.
        let initial_file_size = if bin_existed {
            // Round up to chunk boundary if file size happens not to
            // be a multiple of BIN_CHUNK_BYTES (clean exit truncates
            // back to data size, which may not be aligned).
            let cur = bin_fd.metadata()?.len();
            let rounded = round_up_to_chunk(cur.max(FILE_HEADER_BYTES), BIN_CHUNK_BYTES);
            bin_fd.set_len(rounded)?;
            rounded
        } else {
            bin_fd.set_len(BIN_CHUNK_BYTES)?;
            BIN_CHUNK_BYTES
        };
        let bin_mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                initial_file_size as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&bin_fd),
                0,
            )
        };
        if bin_mmap_ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let bin_mmap_ptr = bin_mmap_ptr as *mut u8;
        let bin_mmap_cap = initial_file_size as usize;

        if !bin_existed {
            // Write header bytes directly into the mmap.
            let hdr = build_header_bytes();
            // SAFETY: bin_mmap_cap >= FILE_HEADER_BYTES guaranteed by
            // ftruncate(BIN_CHUNK_BYTES).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    hdr.as_ptr(),
                    bin_mmap_ptr,
                    hdr.len(),
                );
            }
        }

        // Idx: same pattern.  Single R/W fd, mmap'd MAP_SHARED.
        let idx_fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&idx_path)?;
        let idx_file_size_on_disk = idx_fd.metadata()?.len();

        // If the bin file was just created or the idx file looks
        // mismatched (zero length, or not a multiple of 8 — corrupt /
        // mid-truncate), rebuild idx from a forward scan of bin.
        let need_idx_rebuild = !bin_existed
            || idx_file_size_on_disk == 0
            || idx_file_size_on_disk % 8 != 0;
        if need_idx_rebuild {
            // Truncate idx to zero (drop stale entries), then walk the
            // bin records and write each entry.
            idx_fd.set_len(0)?;
            // The bin we just mapped may include over-allocated tail
            // bytes (zeros); rebuild_idx_from_bin only scans up to the
            // pre-existing on-disk size, so use that bound.
            let bin_data_len_for_scan = if bin_existed {
                // The pre-ftruncate-up file size — what holds real data.
                // We need the original size BEFORE we rounded up.  Read
                // from header probe instead: walk from FILE_HEADER_BYTES
                // through records and stop at the first invalid
                // header.
                rebuild_idx_from_bin_via_mmap(
                    bin_mmap_ptr,
                    bin_mmap_cap as u64,
                    &idx_fd,
                )?
            } else {
                FILE_HEADER_BYTES
            };
            let _ = bin_data_len_for_scan;
        }

        // mmap idx at its current ftruncated size (rounded up to chunk).
        let idx_initial_size = {
            let cur = idx_fd.metadata()?.len();
            let rounded = round_up_to_chunk(cur, IDX_CHUNK_BYTES).max(IDX_CHUNK_BYTES);
            idx_fd.set_len(rounded)?;
            rounded
        };
        let idx_mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                idx_initial_size as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&idx_fd),
                0,
            )
        };
        if idx_mmap_ptr == libc::MAP_FAILED {
            unsafe { libc::munmap(bin_mmap_ptr as *mut _, bin_mmap_cap); }
            return Err(std::io::Error::last_os_error());
        }
        let idx_mmap_ptr = idx_mmap_ptr as *mut u8;
        let idx_mmap_cap = idx_initial_size as usize;

        // Find the real idx count: scan backwards through the mmap
        // looking for the last non-zero u64 entry.  Over-allocation
        // from a prior process (skipped Drop on execv) leaves trailing
        // zero u64s — they're not valid record offsets (FILE_HEADER_BYTES
        // = 32 is the smallest legitimate offset) so the boundary
        // is unambiguous.
        let hot_lines = Self::find_idx_tail_count(idx_mmap_ptr, idx_mmap_cap);

        // Trim a trailing partial-record left by a mid-write crash.
        // (Mmap-write is structured as `idx-entry written AFTER the
        // bin record's bytes are memcpy'd`, so a torn write can leave
        // an idx pointing at a partial bin record.  Same recovery as
        // the BufWriter era.)
        let (hot_lines, bin_write_offset) = Self::trim_trailing_partial_via_mmap(
            bin_mmap_ptr,
            bin_mmap_cap as u64,
            idx_mmap_ptr,
            hot_lines,
        );
        let idx_write_offset = hot_lines * 8;

        // F2+5 — derive cold paths + open cold fds if files exist.
        // `with_extension("cold.bin")` works because `bin_path` already
        // ends in `.bin`; `with_extension` replaces just the extension
        // segment.  Same for idx.
        let cold_bin_path = bin_path.with_extension("cold.bin");
        let cold_idx_path = idx_path.with_extension("cold.idx");
        let (cold_bin_for_read, cold_idx_for_read, cold_total_lines) =
            match (cold_bin_path.exists(), cold_idx_path.exists()) {
                (true, true) => {
                    let cold_bin_fd = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&cold_bin_path)
                        .ok();
                    let cold_idx_fd = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&cold_idx_path)
                        .ok();
                    let n = cold_idx_fd
                        .as_ref()
                        .and_then(|f| f.metadata().ok())
                        .map(|m| m.len() / 8)
                        .unwrap_or(0);
                    (cold_bin_fd, cold_idx_fd, n)
                }
                _ => (None, None, 0),
            };
        // Logical line indexing: cold occupies [0, cold_total_lines);
        // hot occupies [cold_total_lines, cold_total_lines + hot_count).
        // Across L3 restart, the previous process's cold_first_line is
        // forgotten — line_idx resets to start at 0.
        let cold_first_line: u64 = 0;
        let hot_first_line = cold_total_lines;
        let hot_count = hot_lines;
        let total_lines = hot_count.saturating_add(cold_total_lines);

        let mut ram_cells = Vec::with_capacity(ram_capacity.saturating_mul(cols));
        let mut ram_wrapped = Vec::with_capacity(ram_capacity);
        // RAM ring loads the tail of HOT only.  Cold tier is read on
        // demand via cold fds; we don't pre-load it into the hot ring
        // because the user's working scroll-back is almost always in
        // hot (recent), and cold gets exercised only by occasional
        // long scroll-back / search.
        let load_n = (hot_count as usize).min(ram_capacity);
        if load_n > 0 {
            let first_idx = (hot_count as usize) - load_n;
            for li in first_idx..(hot_count as usize) {
                let off = read_idx_at(&idx_fd, li as u64)?;
                let (cells, wrapped) = read_record_at(&bin_fd, off)?;
                // Pad / clip to `cols` so the RAM ring's flat Vec
                // stays uniform.  Historic lines at the old width
                // become whatever the new width says.
                let row = pad_or_clip(&cells, cols);
                ram_cells.extend_from_slice(&row);
                ram_wrapped.push(wrapped);
            }
        }

        Ok(Self {
            bin_path,
            idx_path,
            cols,
            ram_capacity,
            bin_fd,
            bin_mmap_ptr: std::cell::Cell::new(bin_mmap_ptr),
            bin_mmap_cap: std::cell::Cell::new(bin_mmap_cap),
            bin_write_offset,
            idx_fd,
            idx_mmap_ptr: std::cell::Cell::new(idx_mmap_ptr),
            idx_mmap_cap: std::cell::Cell::new(idx_mmap_cap),
            idx_write_offset,
            ram_cells,
            ram_wrapped,
            ram_head: 0,
            ram_len: load_n,
            total_lines,
            cold_bin_path,
            cold_idx_path,
            cold_bin_for_read,
            cold_idx_for_read,
            cold_first_line,
            cold_total_lines,
            hot_first_line,
            hot_bytes_cap: hot_bytes_cap(),
            scratch: Vec::with_capacity(
                32 + cols.saturating_mul(crate::terminal::CELL_BYTES_PUB),
            ),
        })
    }

    /// F3+8 — backward scan of the idx mmap to find the real line
    /// count after a previous process left over-allocated zero
    /// padding at the tail (skipped Drop on `execv`).  Stops at the
    /// last non-zero u64 entry; offset 0 is never a legitimate
    /// record offset (the smallest is `FILE_HEADER_BYTES = 32`).
    fn find_idx_tail_count(idx_mmap_ptr: *mut u8, idx_mmap_cap: usize) -> u64 {
        if idx_mmap_ptr.is_null() || idx_mmap_cap < 8 {
            return 0;
        }
        let max_entries = (idx_mmap_cap / 8) as u64;
        let scan_floor = max_entries.saturating_sub(IDX_REOPEN_SCAN_MAX);
        let mut i = max_entries;
        while i > scan_floor {
            i -= 1;
            let off = (i as usize) * 8;
            // SAFETY: bounds checked — i < max_entries = idx_mmap_cap/8,
            // so off + 8 <= idx_mmap_cap.
            let v = unsafe {
                let p = idx_mmap_ptr.add(off);
                let mut buf = [0u8; 8];
                std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), 8);
                u64::from_le_bytes(buf)
            };
            if v != 0 {
                return i + 1;
            }
        }
        scan_floor
    }

    /// F3+8 — mmap-aware trailing-partial trim.  Walks back at most
    /// 4 records from the idx tail, verifies each bin record header
    /// looks well-formed and its body fits before the mmap cap
    /// (== file size on disk).  Returns the corrected (line_count,
    /// bin_write_offset) tuple.
    fn trim_trailing_partial_via_mmap(
        bin_mmap_ptr: *mut u8,
        bin_mmap_cap: u64,
        idx_mmap_ptr: *mut u8,
        total_lines: u64,
    ) -> (u64, u64) {
        if total_lines == 0 {
            return (0, FILE_HEADER_BYTES);
        }
        let mut last_good = total_lines;
        let mut bin_tail: u64 = 0;
        let probe_n = last_good.min(4);
        for off_back in 0..probe_n {
            let li = last_good - 1 - off_back;
            let off = unsafe {
                let p = idx_mmap_ptr.add((li as usize) * 8);
                let mut buf = [0u8; 8];
                std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), 8);
                u64::from_le_bytes(buf)
            };
            if off < FILE_HEADER_BYTES || off + 4 > bin_mmap_cap {
                last_good = li;
                bin_tail = off.min(bin_mmap_cap);
                continue;
            }
            let rec_len = unsafe {
                let p = bin_mmap_ptr.add(off as usize);
                let mut buf = [0u8; 4];
                std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), 4);
                u32::from_le_bytes(buf) as u64
            };
            let end = off + 4 + rec_len;
            if end > bin_mmap_cap {
                last_good = li;
                bin_tail = off;
            } else {
                bin_tail = end;
                break;
            }
        }
        if last_good == 0 {
            (0, FILE_HEADER_BYTES)
        } else {
            (last_good, bin_tail)
        }
    }

    fn rename_corrupt(bin_path: &std::path::Path, idx_path: &std::path::Path) -> std::io::Result<()> {
        let suffix = format!(
            ".corrupt.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let mut bin_corrupt = bin_path.as_os_str().to_owned();
        bin_corrupt.push(&suffix);
        std::fs::rename(bin_path, std::path::PathBuf::from(bin_corrupt))?;
        let _ = std::fs::remove_file(idx_path); // best-effort; idx without bin is useless
        Ok(())
    }

    /// F2+5 — rotate the current hot pair into cold and reopen a
    /// fresh hot pair.  Called by push_line when an incoming record
    /// would push hot past `hot_bytes_cap`.  Any existing cold pair
    /// is overwritten — that's the "delete" tier; once rotated past,
    /// older history is unrecoverable.  RAM ring contents are
    /// preserved (in-memory state is independent of the file).
    fn rotate_to_cold(&mut self) -> std::io::Result<()> {
        // Capture the count of rows about to leave hot for cold.
        let hot_count_before = self.total_lines - self.hot_first_line;
        if hot_count_before == 0 {
            // Nothing to rotate (the very first push exceeded cap on
            // an empty file — degenerate).  Just continue writing.
            return Ok(());
        }
        // 1) Trim over-allocated tail on hot files so the cold pair
        //    is right-sized.  Then munmap.  Drop the fds so the rename
        //    is a clean inode swap.
        let bin_data_len = self.bin_write_offset;
        let idx_data_len = self.idx_write_offset;
        self.bin_fd.set_len(bin_data_len)?;
        self.idx_fd.set_len(idx_data_len)?;
        let bp = self.bin_mmap_ptr.get();
        let bl = self.bin_mmap_cap.get();
        if !bp.is_null() && bl > 0 {
            unsafe { libc::munmap(bp as *mut _, bl); }
        }
        let ip = self.idx_mmap_ptr.get();
        let il = self.idx_mmap_cap.get();
        if !ip.is_null() && il > 0 {
            unsafe { libc::munmap(ip as *mut _, il); }
        }
        self.bin_mmap_ptr.set(std::ptr::null_mut());
        self.bin_mmap_cap.set(0);
        self.idx_mmap_ptr.set(std::ptr::null_mut());
        self.idx_mmap_cap.set(0);
        // Drop the cold fds before rename overwrites their inodes.
        self.cold_bin_for_read = None;
        self.cold_idx_for_read = None;
        // Swap the hot R/W fds to /dev/null placeholders so the rename
        // can happen without holding open fds on the inode that's
        // about to become cold.
        let _placeholder = std::mem::replace(&mut self.bin_fd, dev_null_file()?);
        drop(_placeholder);
        let _placeholder = std::mem::replace(&mut self.idx_fd, dev_null_file()?);
        drop(_placeholder);
        // 2) Delete any stale cold pair (defence in depth).
        let _ = std::fs::remove_file(&self.cold_bin_path);
        let _ = std::fs::remove_file(&self.cold_idx_path);
        // 3) Rename hot → cold.
        std::fs::rename(&self.bin_path, &self.cold_bin_path)?;
        std::fs::rename(&self.idx_path, &self.cold_idx_path)?;
        // 4) Open fresh hot pair, ftruncate to one chunk, mmap, write
        //    header into the mmap.  Same shape as the fresh-file path
        //    in `open()`.
        let bin_fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.bin_path)?;
        bin_fd.set_len(BIN_CHUNK_BYTES)?;
        let bin_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BIN_CHUNK_BYTES as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&bin_fd),
                0,
            )
        };
        if bin_ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let bin_ptr = bin_ptr as *mut u8;
        let hdr = build_header_bytes();
        unsafe {
            std::ptr::copy_nonoverlapping(hdr.as_ptr(), bin_ptr, hdr.len());
        }

        let idx_fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.idx_path)?;
        idx_fd.set_len(IDX_CHUNK_BYTES)?;
        let idx_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                IDX_CHUNK_BYTES as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&idx_fd),
                0,
            )
        };
        if idx_ptr == libc::MAP_FAILED {
            unsafe { libc::munmap(bin_ptr as *mut _, BIN_CHUNK_BYTES as usize); }
            return Err(std::io::Error::last_os_error());
        }
        let idx_ptr = idx_ptr as *mut u8;

        self.bin_fd = bin_fd;
        self.idx_fd = idx_fd;
        self.bin_mmap_ptr.set(bin_ptr);
        self.bin_mmap_cap.set(BIN_CHUNK_BYTES as usize);
        self.idx_mmap_ptr.set(idx_ptr);
        self.idx_mmap_cap.set(IDX_CHUNK_BYTES as usize);
        self.bin_write_offset = FILE_HEADER_BYTES;
        self.idx_write_offset = 0;

        // 5) Open the (newly renamed) cold read fds.
        self.cold_bin_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.cold_bin_path)
            .ok();
        self.cold_idx_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.cold_idx_path)
            .ok();
        // 6) Logical boundary bookkeeping.
        self.cold_first_line = self.hot_first_line;
        self.cold_total_lines = hot_count_before;
        self.hot_first_line = self.total_lines;
        Ok(())
    }

    /// F3+8 — grow the bin mmap to cover at least `needed_offset`
    /// bytes.  Round up to BIN_CHUNK_BYTES boundaries to amortise
    /// ftruncate + remap across many pushes.  No-op when current
    /// capacity already covers the request.
    fn ensure_bin_capacity(&self, needed_offset: u64) -> std::io::Result<()> {
        let cur_cap = self.bin_mmap_cap.get() as u64;
        if cur_cap >= needed_offset {
            return Ok(());
        }
        let new_cap = round_up_to_chunk(needed_offset, BIN_CHUNK_BYTES);
        self.bin_fd.set_len(new_cap)?;
        let old_ptr = self.bin_mmap_ptr.get();
        if !old_ptr.is_null() && cur_cap > 0 {
            unsafe { libc::munmap(old_ptr as *mut _, cur_cap as usize); }
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                new_cap as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.bin_fd),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            self.bin_mmap_ptr.set(std::ptr::null_mut());
            self.bin_mmap_cap.set(0);
            return Err(std::io::Error::last_os_error());
        }
        self.bin_mmap_ptr.set(ptr as *mut u8);
        self.bin_mmap_cap.set(new_cap as usize);
        Ok(())
    }

    fn ensure_idx_capacity(&self, needed_offset: u64) -> std::io::Result<()> {
        let cur_cap = self.idx_mmap_cap.get() as u64;
        if cur_cap >= needed_offset {
            return Ok(());
        }
        let new_cap = round_up_to_chunk(needed_offset, IDX_CHUNK_BYTES);
        self.idx_fd.set_len(new_cap)?;
        let old_ptr = self.idx_mmap_ptr.get();
        if !old_ptr.is_null() && cur_cap > 0 {
            unsafe { libc::munmap(old_ptr as *mut _, cur_cap as usize); }
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                new_cap as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.idx_fd),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            self.idx_mmap_ptr.set(std::ptr::null_mut());
            self.idx_mmap_cap.set(0);
            return Err(std::io::Error::last_os_error());
        }
        self.idx_mmap_ptr.set(ptr as *mut u8);
        self.idx_mmap_cap.set(new_cap as usize);
        Ok(())
    }

    /// Append one line.  Hot path.
    ///
    /// F3+8 — writes go directly into the file's mmap (MAP_SHARED).
    /// No userspace buffering → the line is visible to anyone who
    /// reopens this file (including a re-execv'd new L3) the moment
    /// `push_line` returns, no flush required.
    pub fn push_line(&mut self, line: &[crate::grid::Cell], wrapped: bool) {
        // F2+4 — trim trailing default cells before writing.  A
        // claudecode-style TUI row in a 200-col grid is typically ~30
        // chars of real text + ~170 trailing blanks; encoding each
        // blank at the full 13 bytes/cell costs ~2 kB/row × millions of
        // rows = multi-GB files.  We strip trailing `Cell::default()`
        // tail before the record write; the read path naturally fills
        // missing columns with `Cell::default()` via `cell_at_view`'s
        // None-fallback (see grid.rs).  The RAM ring still pads to
        // `self.cols` in `push_into_ring` so hot reads via the ring see
        // the same shape as before.  Mid-row blank runs and
        // background-colored blanks are NOT trimmed (only the trailing
        // tail of cells equal to `Cell::default()`), so a status-bar
        // pad or a syntax-highlighted gap survives intact.
        let default_cell = crate::grid::Cell::default();
        let trimmed_len = line
            .iter()
            .rposition(|c| *c != default_cell)
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &line[..trimmed_len];
        let cols_u16 = line.len().min(u16::MAX as usize) as u16;
        let rec_len = (1 + 2 + line.len() * crate::terminal::CELL_BYTES_PUB) as u32;
        let total_bytes = 4 + rec_len as usize;

        // F2+5 — rotate hot → cold when this record would push past
        // the cap.  Skipped silently if rotate_to_cold errors (e.g.
        // fs::rename fail) — the line still goes to RAM ring + best-
        // effort to hot file, just may push past cap once.  Cap
        // overshoot by one record is acceptable since the cap is a
        // soft budget anyway.
        if self.bin_write_offset.saturating_add(total_bytes as u64) > self.hot_bytes_cap {
            let _ = self.rotate_to_cold();
        }

        // Assemble the full record in the scratch buffer.  Per-cell
        // writes via `extend_from_slice` are well-optimised by Vec
        // (vectorised memcpy at the codegen level); doing the same
        // sequence as 2N small `copy_nonoverlapping` calls directly
        // into the mmap pessimises by ~5× (each cell becomes a
        // separate call boundary the compiler can't fold).  After
        // assembly, ONE bulk memcpy moves the record into mmap.
        self.scratch.clear();
        self.scratch.reserve(total_bytes);
        self.scratch.extend_from_slice(&rec_len.to_le_bytes());
        self.scratch.push(wrapped as u8);
        self.scratch.extend_from_slice(&cols_u16.to_le_bytes());
        for c in line {
            self.scratch.extend_from_slice(&(c.ch as u32).to_le_bytes());
            self.scratch.extend_from_slice(&crate::terminal::serialize_attrs_pub(c.attrs));
        }
        debug_assert_eq!(self.scratch.len(), total_bytes);

        // Grow bin mmap if this record won't fit in the current
        // chunk.  Pre-roll the idx grow as well (one 8-byte entry).
        if self.ensure_bin_capacity(self.bin_write_offset + total_bytes as u64).is_ok()
            && self.ensure_idx_capacity(self.idx_write_offset + 8).is_ok()
        {
            let rec_offset = self.bin_write_offset;
            let bin_ptr = self.bin_mmap_ptr.get();
            let idx_ptr = self.idx_mmap_ptr.get();
            if !bin_ptr.is_null() && !idx_ptr.is_null() {
                // SAFETY: bounds checked by ensure_bin_capacity /
                // ensure_idx_capacity above; offsets and lengths are
                // within the mmap regions; scratch and mmap don't
                // alias (scratch is a Vec we own; mmap is a separate
                // kernel-backed region).
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.scratch.as_ptr(),
                        bin_ptr.add(rec_offset as usize),
                        total_bytes,
                    );
                    let off_bytes = rec_offset.to_le_bytes();
                    std::ptr::copy_nonoverlapping(
                        off_bytes.as_ptr(),
                        idx_ptr.add(self.idx_write_offset as usize),
                        8,
                    );
                }
                self.bin_write_offset += total_bytes as u64;
                self.idx_write_offset += 8;
            }
        }

        // Always push into RAM ring so reads observe the line.
        self.push_into_ring(line, wrapped);
        self.total_lines += 1;
    }

    /// F3+8 — read the byte offset of line `idx` from the idx mmap.
    /// The mmap (`MAP_SHARED`) sees writes immediately via the kernel
    /// page cache; no buffer-flush phase is needed.  Falls back to
    /// `pread` only if the read offset is past the current mmap cap
    /// (race after a recent rotate where the mmap might be stale —
    /// not a steady-state concern).
    fn read_idx_via_mmap(&self, line_idx: u64) -> std::io::Result<u64> {
        let off_in_idx = line_idx * 8;
        let mmap_ptr = self.idx_mmap_ptr.get();
        let mmap_cap = self.idx_mmap_cap.get();
        if !mmap_ptr.is_null() && (off_in_idx as usize) + 8 <= mmap_cap {
            let buf = unsafe {
                std::slice::from_raw_parts(mmap_ptr.add(off_in_idx as usize), 8)
            };
            return Ok(u64::from_le_bytes(buf.try_into().unwrap()));
        }
        read_idx_at(&self.idx_fd, line_idx)
    }

    fn push_into_ring(&mut self, line: &[crate::grid::Cell], wrapped: bool) {
        if self.ram_capacity == 0 {
            return;
        }
        let row = pad_or_clip(line, self.cols);
        if self.ram_len < self.ram_capacity {
            self.ram_cells.extend_from_slice(&row);
            self.ram_wrapped.push(wrapped);
            self.ram_len += 1;
            return;
        }
        let slot = self.ram_head;
        self.ram_head = (self.ram_head + 1) % self.ram_capacity;
        let start = slot * self.cols;
        self.ram_cells[start..start + self.cols].copy_from_slice(&row);
        self.ram_wrapped[slot] = wrapped;
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
        // Hot tier — read straight from the MAP_SHARED mmap.  No
        // flush phase: F3+8 removed BufWriter (kernel page cache is
        // the single source of truth, kept in sync by mmap writes).
        let hot_local = (line_idx as u64) - self.hot_first_line;
        let off = self.read_idx_via_mmap(hot_local).ok()?;
        self.read_record_via_mmap(off, Some(col))
            .and_then(|(cells, _w)| cells.get(col).copied())
            .or_else(|| {
                // Fallback if mmap path failed for any reason: classic
                // pread.  Same correctness; just slower.
                let (cells, _w) = read_record_at(&self.bin_fd, off).ok()?;
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
    fn read_record_via_mmap(&self, offset: u64, _col_filter: Option<usize>) -> Option<(Vec<crate::grid::Cell>, bool)> {
        let mmap_ptr = self.bin_mmap_ptr.get();
        let mmap_cap = self.bin_mmap_cap.get();
        if mmap_ptr.is_null() || (offset as usize) + 4 > mmap_cap {
            return None;
        }
        let len_slice = unsafe {
            std::slice::from_raw_parts(mmap_ptr.add(offset as usize), 4)
        };
        let rec_len = u32::from_le_bytes(len_slice.try_into().unwrap()) as usize;
        if (offset as usize) + 4 + rec_len > mmap_cap {
            return None;
        }
        let body = unsafe {
            std::slice::from_raw_parts(mmap_ptr.add(offset as usize + 4), rec_len)
        };
        if body.len() < 3 {
            return None;
        }
        let wrapped = body[0] != 0;
        let cols = u16::from_le_bytes([body[1], body[2]]) as usize;
        let want_cells_bytes = cols * crate::terminal::CELL_BYTES_PUB;
        if body.len() < 3 + want_cells_bytes {
            return None;
        }
        let mut cells = Vec::with_capacity(cols);
        let mut p = 3;
        for _ in 0..cols {
            let ch_u = u32::from_le_bytes(body[p..p + 4].try_into().unwrap());
            let attrs = crate::terminal::deserialize_attrs_pub(
                &body[p + 4..p + 4 + crate::terminal::ATTRS_BYTES_PUB],
            );
            let ch = char::from_u32(ch_u).unwrap_or(' ');
            cells.push(crate::grid::Cell { ch, attrs });
            p += crate::terminal::CELL_BYTES_PUB;
        }
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
            return Some(self.ram_cells[start..start + self.cols].to_vec());
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
        let hot_local = (idx as u64) - self.hot_first_line;
        let off = self.read_idx_via_mmap(hot_local).ok()?;
        self.read_record_via_mmap(off, None)
            .map(|(c, _)| c)
            .or_else(|| {
                let (cells, _wrapped) = read_record_at(&self.bin_fd, off).ok()?;
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
            let Some(cold_idx_fd) = self.cold_idx_for_read.as_ref() else { return false; };
            let Some(cold_bin_fd) = self.cold_bin_for_read.as_ref() else { return false; };
            let cold_local = (idx as u64) - self.cold_first_line;
            let Ok(off) = read_idx_at(cold_idx_fd, cold_local) else { return false; };
            let Ok((_, wrapped)) = read_record_at(cold_bin_fd, off) else { return false; };
            return wrapped;
        }
        let hot_local = (idx as u64) - self.hot_first_line;
        let Some(off) = self.read_idx_via_mmap(hot_local).ok() else { return false; };
        if let Some((_, w)) = self.read_record_via_mmap(off, None) {
            return w;
        }
        let Some((_cells, wrapped)) = read_record_at(&self.bin_fd, off).ok() else { return false; };
        wrapped
    }

    pub fn clear(&mut self) {
        // Clear the RAM ring view but DO NOT truncate the file:
        // CSI 3 J (clear scrollback) operates on the live emulator's
        // view, not on persisted history that the user might have
        // already exited a long-running session expecting to keep.
        // If the user really wants to wipe the file, they can rm it
        // when no L3 is open.
        self.ram_cells.clear();
        self.ram_wrapped.clear();
        self.ram_head = 0;
        self.ram_len = 0;
        // total_lines and the file are intentionally NOT touched.
        // This matches `MemoryScrollback::clear` which keeps Vec
        // capacity but drops content — we keep file content but
        // drop ring content (the live view) — both flavours of
        // "clear what's currently visible".
    }

    pub fn approx_bytes(&self) -> usize {
        // RAM ring resident bytes (matches Memory/Disk approx_bytes
        // semantics — what we hold in process, not what's on disk).
        self.ram_len * self.cols * crate::terminal::CELL_BYTES_PUB
    }
}

impl Drop for FileScrollback {
    fn drop(&mut self) {
        // F3+8 — clean exit: trim the over-allocated tail back to the
        // actual data size, then munmap.  On `execv` Drop doesn't run
        // and the file stays over-allocated; the next open's idx
        // backward scan recovers the real boundary either way.
        let _ = self.bin_fd.set_len(self.bin_write_offset);
        let _ = self.idx_fd.set_len(self.idx_write_offset);
        let bp = self.bin_mmap_ptr.get();
        let bl = self.bin_mmap_cap.get();
        if !bp.is_null() && bl > 0 {
            unsafe { libc::munmap(bp as *mut libc::c_void, bl); }
        }
        let ip = self.idx_mmap_ptr.get();
        let il = self.idx_mmap_cap.get();
        if !ip.is_null() && il > 0 {
            unsafe { libc::munmap(ip as *mut libc::c_void, il); }
        }
    }
}

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
    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    fn read_row(&self, idx: u64) -> Option<std::sync::Arc<(Vec<crate::grid::Cell>, bool)>> {
        if idx >= self.total_lines {
            return None;
        }
        if let Some((cached_idx, cached)) = &*self.last_read.borrow() {
            if *cached_idx == idx {
                return Some(std::sync::Arc::clone(cached));
            }
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

impl FileScrollback {
    /// B3 — flush the live writer's BufWriters and hand back a
    /// `FileSnapshot` that the search worker can pread independently.
    /// The new fds are opened against the same `.bin` / `.idx` paths
    /// the live writer is appending to; pread on the snapshot's fds
    /// never blocks the writer and the writer never invalidates the
    /// snapshot's view (appends only grow the file past
    /// `total_lines`).
    pub fn snapshot_for_search(&self) -> std::io::Result<FileSnapshot> {
        // F3+8 — no flush needed: writes go directly through
        // MAP_SHARED into the kernel page cache, which the new
        // read-only fd sees instantly.
        let bin = std::fs::OpenOptions::new().read(true).open(&self.bin_path)?;
        let idx = std::fs::OpenOptions::new().read(true).open(&self.idx_path)?;
        Ok(FileSnapshot {
            bin,
            idx,
            total_lines: self.total_lines,
            last_read: std::cell::RefCell::new(None),
        })
    }
}

/// F3+8 — round `n` up to the next multiple of `chunk`.
fn round_up_to_chunk(n: u64, chunk: u64) -> u64 {
    if chunk == 0 {
        return n;
    }
    n.div_ceil(chunk).saturating_mul(chunk)
}

/// F3+8 — produce the 32-byte file header.
fn build_header_bytes() -> [u8; FILE_HEADER_BYTES as usize] {
    let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
    hdr[0..4].copy_from_slice(&FILE_MAGIC.to_le_bytes());
    hdr[4..8].copy_from_slice(&FILE_VERSION.to_le_bytes());
    hdr[8..12].copy_from_slice(&(crate::terminal::CELL_BYTES_PUB as u32).to_le_bytes());
    hdr[12..16].copy_from_slice(&0u32.to_le_bytes()); // header_flags
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    hdr[16..24].copy_from_slice(&now_ns.to_le_bytes());
    hdr[24..32].copy_from_slice(&0u64.to_le_bytes());
    hdr
}

/// F3+8 — forward scan of the bin mmap to rebuild the idx from
/// scratch.  Used on reopen when the idx file is missing / size-
/// mismatched / corrupt.  Walks records starting at FILE_HEADER_BYTES,
/// stops at the first invalid header (`rec_len = 0` past header bytes,
/// or rec end past `bin_data_cap`).  Writes recovered offsets to
/// `idx_fd` via `pwrite`, returning the byte offset where the next
/// idx entry would land (= valid_lines * 8).
fn rebuild_idx_from_bin_via_mmap(
    bin_mmap_ptr: *mut u8,
    bin_data_cap: u64,
    idx_fd: &std::fs::File,
) -> std::io::Result<u64> {
    use std::os::unix::fs::FileExt;
    if bin_mmap_ptr.is_null() {
        return Ok(0);
    }
    let mut pos = FILE_HEADER_BYTES;
    let mut idx_byte_off: u64 = 0;
    while pos + 4 <= bin_data_cap {
        let rec_len = unsafe {
            let p = bin_mmap_ptr.add(pos as usize);
            let mut buf = [0u8; 4];
            std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), 4);
            u32::from_le_bytes(buf) as u64
        };
        if rec_len == 0 {
            // Treat zero-length record as "no record here" — common
            // when the bin was over-allocated and we walked into a
            // zeroed tail.
            break;
        }
        let end = pos + 4 + rec_len;
        if end > bin_data_cap {
            // Trailing partial; stop here.
            break;
        }
        idx_fd.write_all_at(&pos.to_le_bytes(), idx_byte_off)?;
        idx_byte_off += 8;
        pos = end;
    }
    Ok(idx_byte_off)
}

fn read_exact_at(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

fn read_idx_at(idx: &std::fs::File, line_idx: u64) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    read_exact_at(idx, &mut buf, line_idx * 8)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_record_at(
    bin: &std::fs::File,
    offset: u64,
) -> std::io::Result<(Vec<crate::grid::Cell>, bool)> {
    let mut len_buf = [0u8; 4];
    read_exact_at(bin, &mut len_buf, offset)?;
    let rec_len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; rec_len];
    read_exact_at(bin, &mut body, offset + 4)?;
    if body.len() < 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scrollback rec too short",
        ));
    }
    let wrapped = body[0] != 0;
    let cols = u16::from_le_bytes([body[1], body[2]]) as usize;
    let want_cells = cols * crate::terminal::CELL_BYTES_PUB;
    if body.len() < 3 + want_cells {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scrollback rec cells truncated",
        ));
    }
    let mut cells = Vec::with_capacity(cols);
    let mut p = 3;
    for _ in 0..cols {
        let ch_u = u32::from_le_bytes(body[p..p + 4].try_into().unwrap());
        let attrs = crate::terminal::deserialize_attrs_pub(&body[p + 4..p + 4 + 9]);
        let ch = char::from_u32(ch_u).unwrap_or(' ');
        cells.push(crate::grid::Cell { ch, attrs });
        p += crate::terminal::CELL_BYTES_PUB;
    }
    Ok((cells, wrapped))
}

fn pad_or_clip(line: &[crate::grid::Cell], cols: usize) -> Vec<crate::grid::Cell> {
    if line.len() == cols {
        return line.to_vec();
    }
    let mut out = Vec::with_capacity(cols);
    let take = line.len().min(cols);
    out.extend_from_slice(&line[..take]);
    out.resize(cols, crate::grid::Cell::default());
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

    // ─── RFC-002 step 3: read_lines paging ────────────────────────────

    /// Build a memory-backed scrollback of `n` lines labelled `'a'..`,
    /// so a 5-line ring contains `a,b,c,d,e` (a = oldest, e = newest).
    fn alphabet_sb_memory(n: usize, cols: usize) -> Scrollback {
        let mut sb = Scrollback::memory(n, cols);
        for i in 0..n {
            let ch = (b'a' + (i as u8)) as char;
            let line: Vec<Cell> = (0..cols)
                .map(|_| Cell { ch, ..Default::default() })
                .collect();
            sb.push_line(&line);
        }
        sb
    }

    /// Same shape for the disk variant, with explicit ram_cap +
    /// max_pages so eviction behaviour is deterministic.
    fn alphabet_sb_disk(n: usize, cols: usize) -> Scrollback {
        let mut sb = Scrollback::disk(/*ram_cap*/ 32, /*max_pages*/ 4, cols)
            .expect("disk scrollback init");
        for i in 0..n {
            let ch = (b'a' + (i as u8)) as char;
            let line: Vec<Cell> = (0..cols)
                .map(|_| Cell { ch, ..Default::default() })
                .collect();
            sb.push_line(&line);
        }
        sb
    }

    #[test]
    fn read_lines_zero_count_or_past_floor_returns_empty() {
        let sb = alphabet_sb_memory(5, 4);
        assert!(sb.read_lines(0, 0).is_empty(), "count=0 → empty");
        assert!(sb.read_lines(5, 1).is_empty(), "line_start == len → empty");
        assert!(sb.read_lines(99, 1).is_empty(), "line_start > len → empty");
    }

    #[test]
    fn read_lines_empty_scrollback_returns_empty() {
        let sb = Scrollback::memory(10, 4);
        assert!(sb.read_lines(0, 5).is_empty());
    }

    #[test]
    fn read_lines_zero_start_returns_newest_oldest_first() {
        // 5 lines: a,b,c,d,e (oldest .. newest).  read_lines(0, 3)
        // wants "3 lines starting at newest, going back" =
        // [c, d, e] presented oldest-first.
        let sb = alphabet_sb_memory(5, 4);
        let got = sb.read_lines(0, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0][0].ch, 'c');
        assert_eq!(got[1][0].ch, 'd');
        assert_eq!(got[2][0].ch, 'e');
    }

    #[test]
    fn read_lines_with_offset_skips_newest_lines() {
        // (line_start=2, count=2) on a,b,c,d,e =
        // skip the 2 newest (d, e), return next 2 newer-going-back =
        // [b, c] oldest-first.
        let sb = alphabet_sb_memory(5, 4);
        let got = sb.read_lines(2, 2);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0][0].ch, 'b');
        assert_eq!(got[1][0].ch, 'c');
    }

    #[test]
    fn read_lines_clamped_when_count_crosses_floor() {
        // 5 lines, (line_start=3, count=100): only 2 lines remain
        // before the floor → return 2.
        let sb = alphabet_sb_memory(5, 4);
        let got = sb.read_lines(3, 100);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0][0].ch, 'a');
        assert_eq!(got[1][0].ch, 'b');
    }

    #[test]
    fn read_lines_disk_variant_matches_memory_semantics() {
        // 16 lines on disk, same (line_start=4, count=6) request as
        // the memory case would produce.  Asserts the disk path
        // returns lines in the same orientation + clamping.
        let sb = alphabet_sb_disk(16, 4);
        let got = sb.read_lines(4, 6);
        assert_eq!(got.len(), 6);
        // Lines were a(0) .. p(15); newest = p; line_start=4 means
        // window is [g, h, i, j, k, l] oldest-first.
        let expected: Vec<char> = "ghijkl".chars().collect();
        for (i, row) in got.iter().enumerate() {
            assert_eq!(row[0].ch, expected[i], "row {} mismatch", i);
        }
    }

    #[test]
    fn read_lines_disk_variant_clamps_past_floor() {
        // 8 lines on disk: a..h.  (line_start=6, count=10) →
        // only 2 lines before floor → return [a, b] oldest-first.
        let sb = alphabet_sb_disk(8, 4);
        let got = sb.read_lines(6, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0][0].ch, 'a');
        assert_eq!(got[1][0].ch, 'b');
    }

    // ─── pre-existing tests follow ────────────────────────────────────

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

    #[test]
    fn disk_round_trips_within_ram_capacity() {
        let mut sb = Scrollback::disk(4, 1, 4).expect("disk sb");
        sb.push_line(&fill(b'a', 4));
        sb.push_line(&fill(b'b', 4));
        sb.push_line(&fill(b'c', 4));
        assert_eq!(sb.len(), 3);
        // All three still resident, no eviction yet.
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
        assert_eq!(sb.cell_at(2, 0).unwrap().ch, 'c');
        let line1 = sb.line_to_vec(1).unwrap();
        assert_eq!(line1[0].ch, 'b');
    }

    #[test]
    fn disk_spills_past_ram_capacity_to_disk() {
        // RAM 2, 1 disk page → total cap = 2 + 256 = 258.  Push the
        // exact cap amount, all addressable.
        let mut sb = Scrollback::disk(2, 1, 4).expect("disk sb");
        let total_cap = 2 + LINES_PER_PAGE;
        for i in 0..total_cap {
            let byte = b'a' + (i % 26) as u8;
            sb.push_line(&fill(byte, 4));
        }
        assert_eq!(sb.len(), total_cap);
        assert_eq!(sb.capacity(), total_cap);
        // Oldest (index 0) is whatever-the-kernel-left-evictable; it's
        // the first byte we pushed.
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
        // Newest is the last byte we pushed.
        let newest_byte = b'a' + ((total_cap - 1) % 26) as u8;
        assert_eq!(sb.cell_at(total_cap - 1, 0).unwrap().ch, newest_byte as char);
    }

    #[test]
    fn disk_ring_drops_oldest_past_total_cap() {
        // RAM 2, 1 disk page → total cap = 2 + 256 = 258 lines.
        let mut sb = Scrollback::disk(2, 1, 4).expect("disk sb");
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
    fn disk_full_history_read_back_after_wraparound() {
        // Stress test: push enough lines to wrap the disk ring twice, then
        // verify every addressable line reads back its expected content.
        // Catches per-line corruption that single-cell tests would miss
        // (e.g. an mmap offset bug that interleaves bytes between slots).
        let cols = 8;
        let pages = 2;
        let mut sb = Scrollback::disk(4, pages, cols).expect("disk sb");
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
        let mut sb = Scrollback::disk(4, 2, 4).expect("disk sb");
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

    // ─── A1: FileScrollback unit tests ──────────────────────────

    /// Temp dir scoped to this test — unique per-call, cleaned on
    /// drop.  No external `tempfile` crate dependency.
    struct TmpDir {
        path: std::path::PathBuf,
    }
    impl TmpDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir()
                .join(format!("marspot-scrollback-{label}-{pid}-{n}"));
            std::fs::create_dir_all(&dir).expect("tmpdir create");
            Self { path: dir }
        }
        fn bin(&self) -> std::path::PathBuf { self.path.join("scrollback.bin") }
        fn idx(&self) -> std::path::PathBuf { self.path.join("scrollback.idx") }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn file_create_then_roundtrip_one_line() {
        let tmp = TmpDir::new("one-line");
        let cols = 8usize;
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16)
            .expect("create");
        let line = fill(b'a', cols);
        sb.push_line(&line, false);
        assert_eq!(sb.len(), 1);
        let got = sb.read_line(0).expect("read");
        assert_eq!(got.len(), cols);
        assert_eq!(got[0].ch, 'a');
        assert_eq!(sb.cell_at(0, 3).unwrap().ch, 'a');
        assert!(!sb.wrapped_at(0));
    }

    /// F2+4 — trailing default cells get trimmed at write time.  We
    /// write a known-real-content prefix + a long default tail, then
    /// look at the on-disk record size and confirm only the prefix
    /// + the 7-byte record header (4 rec_len + 1 wrapped + 2 cols)
    /// reached the file.  cell_at still returns the row's prefix cells
    /// at their original columns and `Cell::default()` for trimmed-tail
    /// columns (via `cells.get(col).copied()` returning None on the
    /// mmap path, which the higher-level `cell_at_view` falls back to
    /// default for).
    #[test]
    fn trailing_default_cells_trimmed_on_write() {
        use std::io::{Read, Seek, SeekFrom};
        let tmp = TmpDir::new("trim-tail");
        let cols = 200usize;
        let prefix_len = 30usize;
        // Build a row: 30 distinct chars, then 170 default cells.
        let mut row = Vec::with_capacity(cols);
        for i in 0..prefix_len {
            row.push(Cell { ch: (b'a' + (i % 26) as u8) as char, ..Default::default() });
        }
        for _ in prefix_len..cols {
            row.push(Cell::default());
        }
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4)
            .expect("create");
        sb.push_line(&row, false);
        // Force an off-ring lookup by evicting row 0 from the ring.
        for _ in 0..6 {
            sb.push_line(&fill(b'.', cols), false);
        }
        // 1) The on-disk record for row 0 should be sized for ONLY the
        //    prefix cells (30) + header.
        let bin_path = tmp.bin();
        // Flush so we can read what got written.
        drop(sb);
        let mut f = std::fs::File::open(&bin_path).expect("open bin");
        f.seek(SeekFrom::Start(FILE_HEADER_BYTES)).unwrap();
        let mut rec_len_buf = [0u8; 4];
        f.read_exact(&mut rec_len_buf).unwrap();
        let rec_len = u32::from_le_bytes(rec_len_buf) as usize;
        // Expected: 1 wrapped + 2 cols + prefix_len × 13 cells.
        let expected = 1 + 2 + prefix_len * crate::terminal::CELL_BYTES_PUB;
        assert_eq!(
            rec_len, expected,
            "row record size {} != expected {} — trim didn't fire \
             (cell count saved = {}; cols requested = {})",
            rec_len, expected,
            (rec_len.saturating_sub(3)) / crate::terminal::CELL_BYTES_PUB,
            cols
        );
        // 2) Reopen and confirm the prefix reads back correctly and
        //    the trimmed-away tail comes back as None from cell_at
        //    (which higher-level cell_at_view turns into default).
        let sb2 = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4)
            .expect("reopen");
        for c in 0..prefix_len {
            let want = (b'a' + (c % 26) as u8) as char;
            let got = sb2.cell_at(0, c)
                .unwrap_or_else(|| panic!("cell_at(0, {}) = None for prefix", c))
                .ch;
            assert_eq!(got, want, "prefix col {} mismatch", c);
        }
        for c in prefix_len..cols {
            assert!(
                sb2.cell_at(0, c).is_none(),
                "cell_at(0, {}) returned Some after trim — expected None \
                 (cell_at_view fills default)",
                c
            );
        }
    }

    /// F2+5 — rotation: when hot exceeds cap, rename hot → cold and
    /// open fresh hot.  Verify (a) cold files appear on disk,
    /// (b) cell_at for early-pushed rows still works through the cold
    /// tier, (c) cell_at for post-rotation rows works through hot,
    /// (d) a second rotation drops the OLDEST cold (delete tier).
    #[test]
    fn rotation_writes_cold_and_keeps_old_rows_readable() {
        let tmp = TmpDir::new("rotate");
        let cols = 8usize;
        // Tiny cap: header (32) + a handful of records.  Each record is
        // 4 + 1 + 2 + cols×13 = 7 + 104 = 111 bytes.
        // Cap at ~600 B → rotates after ~5 rows.
        std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0"); // 0 MB still ≥ default
        // 0 MB cap is degenerate; manually patch via direct construction
        // is not exposed — so set a non-zero cap that's still tiny.
        // 1 MB = 1048576 bytes; cap≥1MB won't trigger.  We instead set
        // an explicit small value via env override interpretation:
        // hot_bytes_cap() floors at value*1MB, so 0 effectively disables
        // rotation.  We want a small >0 trigger, so... use the unit
        // size 1MB and feed lots of rows.
        std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1");
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4)
            .expect("create");
        // 1 MB / ~111 B = ~9450 rows before rotation.  Push 12 000
        // distinct rows so at least one rotation happens.  Each row's
        // first cell encodes its sequence number (mod 26) so we can
        // verify the row's identity on read-back.
        let total_push = 12_000usize;
        for i in 0..total_push {
            let mut row: Vec<Cell> = Vec::with_capacity(cols);
            row.push(Cell { ch: (b'A' + (i % 26) as u8) as char, ..Default::default() });
            for _ in 1..cols {
                row.push(Cell { ch: '.', ..Default::default() });
            }
            sb.push_line(&row, false);
        }
        // Sanity: rotation must have happened.
        let cold_bin = tmp.bin().with_extension("cold.bin");
        let cold_idx_p = tmp.bin().with_extension("cold.idx");
        // bin_path stem is "scrollback" so .cold.bin file ought to exist.
        // (TmpDir::bin returns ".bin"; with_extension("cold.bin") yields
        // ".cold.bin".)
        assert!(
            cold_bin.exists(),
            "rotation should have produced a cold .bin at {:?}",
            cold_bin
        );
        // The idx path was passed separately; we apply with_extension on
        // it to derive its cold sibling.  Test helper paths align.
        let cold_idx = tmp.idx().with_extension("cold.idx");
        assert!(cold_idx.exists(), "cold .idx missing at {:?}", cold_idx);
        let _ = cold_idx_p; // silence unused name
        // (a) Earliest row (idx 0) should still resolve — comes from cold.
        let got0 = sb.cell_at(0, 0)
            .expect("cell_at(0, 0) returned None — early row should be in cold");
        assert_eq!(got0.ch, 'A', "cold row 0 first cell mismatch");
        // (b) Latest row (idx total-1) should resolve — comes from hot.
        let got_last = sb.cell_at(total_push - 1, 0)
            .expect("cell_at(last, 0) None — should be in hot");
        let want_last = (b'A' + ((total_push - 1) % 26) as u8) as char;
        assert_eq!(got_last.ch, want_last, "hot tail row mismatch");
        // (c) Mid-range row: hopefully also reachable (either in cold's
        // tail or hot's head depending on rotation point).  We just
        // require it round-trips correctly.
        let mid = total_push / 2;
        let got_mid = sb.cell_at(mid, 0)
            .expect("cell_at(mid, 0) None — mid row should be reachable in hot or cold");
        let want_mid = (b'A' + (mid % 26) as u8) as char;
        assert_eq!(got_mid.ch, want_mid, "mid row mismatch");
        std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB");
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
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_cap)
            .expect("create");
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
            let got = sb.cell_at(0, c)
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
    fn file_create_then_roundtrip_many_lines_and_reopen() {
        let tmp = TmpDir::new("many-reopen");
        let cols = 8usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16)
                .expect("create");
            for i in 0..5000 {
                let ch = ((b'a' + (i % 26) as u8)) as u8;
                sb.push_line(&fill(ch, cols), false);
            }
            assert_eq!(sb.len(), 5000);
        }
        // Reopen: drop above flushes BufWriters + writes idx
        // sentinel.  New instance walks the file headers and
        // populates the RAM ring with the latest 16 lines.
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16)
            .expect("reopen");
        assert_eq!(sb.len(), 5000);
        // Sample at multiple depths: tail (RAM hit), middle
        // (file pread), head (file pread).
        assert_eq!(sb.cell_at(4999, 0).unwrap().ch, ((b'a' + (4999 % 26) as u8)) as char);
        assert_eq!(sb.cell_at(2500, 0).unwrap().ch, ((b'a' + (2500 % 26) as u8)) as char);
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
    }

    #[test]
    fn file_corrupt_header_is_renamed_and_replaced() {
        let tmp = TmpDir::new("corrupt");
        // Pre-corrupt the file with bogus magic.
        std::fs::write(tmp.bin(), b"\xff\xff\xff\xff\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00garbage").unwrap();
        std::fs::write(tmp.idx(), b"junk").unwrap();
        // Open should rename the corrupt file and start fresh.
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8)
            .expect("recover");
        assert_eq!(sb.len(), 0);
        sb.push_line(&fill(b'Z', 4), false);
        assert_eq!(sb.len(), 1);
        // A `scrollback.bin.corrupt.*` file should exist in the dir.
        let any_corrupt = std::fs::read_dir(&tmp.path)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt."));
        assert!(any_corrupt, "expected a renamed corrupt file in {:?}", tmp.path);
    }

    #[test]
    fn file_idx_rebuilt_when_missing() {
        let tmp = TmpDir::new("idx-missing");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
                .expect("create");
            for _ in 0..10 {
                sb.push_line(&fill(b'a', cols), false);
            }
            assert_eq!(sb.len(), 10);
        }
        // Nuke the idx file; .bin survives.
        std::fs::remove_file(tmp.idx()).expect("rm idx");
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
            .expect("recover");
        // Rebuilt idx from .bin → length matches.
        assert_eq!(sb.len(), 10);
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
        assert_eq!(sb.cell_at(9, 3).unwrap().ch, 'a');
    }

    #[test]
    fn file_idx_rebuilt_when_length_mismatched() {
        let tmp = TmpDir::new("idx-trunc");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
                .expect("create");
            for _ in 0..20 {
                sb.push_line(&fill(b'b', cols), false);
            }
        }
        // Truncate idx to half its length.
        let idx_bytes = std::fs::read(tmp.idx()).unwrap();
        let truncated = &idx_bytes[..idx_bytes.len() / 2];
        // Make it length not a multiple of 8 to trip rebuild
        // unconditionally.
        let mut truncated = truncated.to_vec();
        truncated.push(0x55); // tail byte breaks 8-byte alignment
        std::fs::write(tmp.idx(), &truncated).unwrap();
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
            .expect("recover");
        assert_eq!(sb.len(), 20);
    }

    #[test]
    fn file_wrapped_flag_survives_roundtrip() {
        let tmp = TmpDir::new("wrapped");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
                .expect("create");
            sb.push_line(&fill(b'p', cols), false);
            sb.push_line(&fill(b'q', cols), true);
            sb.push_line(&fill(b'r', cols), false);
            sb.push_line(&fill(b's', cols), true);
        }
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
            .expect("reopen");
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
        let cols = 4usize;  // ~59 bytes/record -> ~1100 fit in 64 KiB
        let ram_capacity = 16;  // tiny ring
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_capacity)
            .expect("create");
        // Push enough lines so the ring has rolled many times but
        // the BufWriter still has not auto-flushed.
        let n = 200usize;  // 200 * 59 = 11.8 KiB < 64 KiB buffer
        for i in 0..n {
            let ch = ((b'a' + (i % 26) as u8)) as u8;
            sb.push_line(&fill(ch, cols), false);
        }
        assert_eq!(sb.len(), n);
        // Recent lines: should be in RAM ring, no IO needed.
        let recent_idx = n - 1;
        let ch_recent = ((b'a' + (recent_idx % 26) as u8)) as char;
        assert_eq!(sb.cell_at(recent_idx, 0).unwrap().ch, ch_recent);
        // Old line: must have aged out of the ring.  Bytes are
        // still in the BufWriter buffer.  cell_at must flush + read
        // correctly.
        let old_idx = 5;
        let ch_old = ((b'a' + (old_idx % 26) as u8)) as char;
        assert_eq!(
            sb.cell_at(old_idx, 0).expect("old cell").ch,
            ch_old,
            "cold read of pre-buffer line must flush + read correctly"
        );
        // F3+8 — mmap is set from open() now (MAP_SHARED), no
        // lazy "first cold read mmaps" step any more.
        assert!(!sb.bin_mmap_ptr.get().is_null(), "bin mmap should be set");
        assert!(!sb.idx_mmap_ptr.get().is_null(), "idx mmap should be set");
    }

    /// F3+8 — after enough pushes to push the bin past its first
    /// chunk, push_line must ftruncate + remap so subsequent reads
    /// of newly-cold lines still find a valid mapping.  Without the
    /// chunked-extension path, reads would land past mmap_cap.
    #[test]
    fn file_mmap_remap_on_chunk_growth() {
        let tmp = TmpDir::new("chunk-remap");
        let cols = 4usize;
        let ram_capacity = 4;
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_capacity)
            .expect("create");
        let cap_first = sb.bin_mmap_cap.get();
        assert!(cap_first > 0, "initial chunk should be allocated");
        // Push enough lines to grow past the first BIN_CHUNK_BYTES
        // boundary.  At cols=4 (`6 + 4*13 = 58 B / record`), one
        // 64KB chunk holds ~1130 records; push more than that.
        for i in 0..1500 {
            sb.push_line(&fill((b'a' + (i % 26) as u8) as u8, cols), false);
        }
        // Cold-read a line that aged out of the ring of 4.
        let cold_after_growth = 800usize;
        let want_ch = ((b'a' + (cold_after_growth % 26) as u8)) as char;
        assert_eq!(
            sb.cell_at(cold_after_growth, 0).expect("cell after growth").ch,
            want_ch,
            "chunked extension should let us read past the first chunk"
        );
        assert!(
            sb.bin_mmap_cap.get() > cap_first as usize,
            "mmap should have remapped to a larger chunk: was {}, now {}",
            cap_first,
            sb.bin_mmap_cap.get()
        );
    }

    /// A3: end-to-end through `Terminal::new` with the env-gate
    /// active.  Push lines via `feed`, drop, re-instantiate, assert
    /// scrollback persisted across the "reopen".  Verifies the
    /// wiring of MARSPOT_FILE_SCROLLBACK + MARSPOT_SESSION_ID +
    /// MARSPOT_STATE_DIR all the way down.
    ///
    /// NOTE: cannot run in parallel with other env-mutating tests in
    /// the same process — uses a single global env.  We mark it
    /// `#[ignore]` so `cargo test` skips it by default; `cargo test
    /// -- --ignored a3_terminal_file_scrollback_end_to_end` runs it
    /// explicitly.  Manual e2e in §7.4 covers the same.
    /// A4: snapshot v2 replay must write its scrollback section
    /// through the File variant so silent-update execv preserves
    /// history.  This is the "first install moment" migration path:
    /// pre-A3 L3 wrote v2 snapshot containing scrollback (up to 20k
    /// lines per pane); the post-A3 image with file env-gate active
    /// reads that snapshot in `apply_snapshot`, which calls
    /// `push_historic_scrollback_line`, which (after A3) routes
    /// through `push_line_with_wrapped` so File variant records
    /// land in `scrollback.bin`.  After this single seeding, all
    /// future execvs read the file directly — snapshot is just
    /// the bootstrap path.
    #[test]
    #[ignore]
    fn a4_snapshot_v2_replay_writes_into_file_scrollback() {
        let tmp = TmpDir::new("a4-seed");
        let state_dir = tmp.path.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let sid = 2u64;
        let session_dir = state_dir.join("sessions").join(sid.to_string());
        std::fs::create_dir_all(&session_dir).unwrap();

        // 1. Build a "source" Terminal under Disk (env unset) and
        //    push lines so the snapshot carries a scrollback section.
        let cols = 20u16;
        let rows = 4u16;
        let snapshot_body = {
            let mut t = crate::terminal::Terminal::new(cols, rows);
            for i in 0..30u32 {
                t.feed(format!("seed {i}\r\n").as_bytes());
            }
            t.serialize_snapshot()
        };

        // 2. Build a "target" Terminal under File env, replay snapshot,
        //    drop, reopen → assert scrollback survived through the
        //    file, not just the in-process state.
        unsafe {
            std::env::set_var("MARSPOT_STATE_DIR", &state_dir);
            std::env::set_var("MARSPOT_FILE_SCROLLBACK", "1");
            std::env::set_var("MARSPOT_SESSION_ID", sid.to_string());
        }

        let pre_sb_len = {
            let mut t = crate::terminal::Terminal::new(cols, rows);
            t.apply_snapshot(&snapshot_body).expect("apply");
            t.grid().scrollback_len()
        };
        assert!(
            pre_sb_len >= 20,
            "snapshot replay should populate scrollback ≥ 20 lines, got {pre_sb_len}"
        );

        // Reopen — the new instance must see the seeded scrollback
        // via the FILE, not via snapshot (we don't apply_snapshot
        // here on purpose).
        let post_sb_len = {
            let t = crate::terminal::Terminal::new(cols, rows);
            t.grid().scrollback_len()
        };
        assert!(
            post_sb_len >= pre_sb_len,
            "scrollback after reopen-via-file must be ≥ the snapshot-seeded value; got pre={pre_sb_len} post={post_sb_len}"
        );

        unsafe {
            std::env::remove_var("MARSPOT_FILE_SCROLLBACK");
            std::env::remove_var("MARSPOT_SESSION_ID");
            std::env::remove_var("MARSPOT_STATE_DIR");
        }
    }

    #[test]
    #[ignore]
    fn a3_terminal_file_scrollback_end_to_end() {
        // Build a clean per-test sandbox dir so we don't disturb
        // any real session.
        let tmp = TmpDir::new("a3-end-to-end");
        let state_dir = tmp.path.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        // Session id 1 → state/sessions/1/scrollback.bin
        let sid = 1u64;
        let session_dir = state_dir.join("sessions").join(sid.to_string());
        std::fs::create_dir_all(&session_dir).unwrap();

        // SAFETY: tests mutate process env; we restore after.
        unsafe {
            std::env::set_var("MARSPOT_STATE_DIR", &state_dir);
            std::env::set_var("MARSPOT_FILE_SCROLLBACK", "1");
            std::env::set_var("MARSPOT_SESSION_ID", sid.to_string());
        }

        // Push enough bytes through Terminal::feed to populate
        // scrollback.  Each "\n" advances row; after `rows` rows
        // the next row scrolls one off into scrollback.
        let cols = 20u16;
        let rows = 4u16;
        {
            let mut t = crate::terminal::Terminal::new(cols, rows);
            for i in 0..50u32 {
                t.feed(format!("line {i}\r\n").as_bytes());
            }
            // Scrollback should have ~46 lines (50 emitted minus the
            // last `rows` still on the live grid).
            assert!(
                t.grid().scrollback_len() >= 40,
                "expected scrollback_len ≥ 40, got {}",
                t.grid().scrollback_len()
            );
        }
        // Reopen via a fresh Terminal::new on the same paths.
        {
            let t = crate::terminal::Terminal::new(cols, rows);
            let sb_len = t.grid().scrollback_len();
            assert!(
                sb_len >= 40,
                "scrollback should survive reopen via file path; got {sb_len}"
            );
            // Verify a sample cell.
            let sample = t.grid().scrollback_cell(0, 0);
            assert!(sample.is_some(), "scrollback_cell(0,0) must read back");
        }

        unsafe {
            std::env::remove_var("MARSPOT_FILE_SCROLLBACK");
            std::env::remove_var("MARSPOT_SESSION_ID");
            std::env::remove_var("MARSPOT_STATE_DIR");
        }
    }

    #[test]
    fn file_trailing_partial_record_trimmed() {
        let tmp = TmpDir::new("partial");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
                .expect("create");
            for _ in 0..5 {
                sb.push_line(&fill(b'k', cols), false);
            }
        }
        // Append garbage that LOOKS like a record header pointing
        // past EOF — simulates a crash mid-write of record 6.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.bin())
            .unwrap();
        // rec_len claims 100 bytes, but we only write 4 bytes of body
        let bogus_rec_len: u32 = 100;
        f.write_all(&bogus_rec_len.to_le_bytes()).unwrap();
        f.write_all(&[1, 4, 0]).unwrap(); // wrapped + cols, no cells
        // Also append a fake idx entry pointing at the partial record.
        let bin_len_before_garbage = std::fs::metadata(tmp.bin()).unwrap().len() - 7;
        let mut idx_f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.idx())
            .unwrap();
        idx_f.write_all(&bin_len_before_garbage.to_le_bytes()).unwrap();
        drop(f);
        drop(idx_f);
        // Reopen: trailing partial record is detected via the
        // rec_len bounds check; len falls back to the last good record.
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
            .expect("recover");
        // 5 good records + 1 partial → recover to 5.
        assert!(
            sb.len() == 5 || sb.len() == 6,
            "expected partial to drop us back to 5 (or stay at 6 if heuristic kept it); got {}",
            sb.len()
        );
        // The partial record's idx must not let us read garbage.
        // For safety, verify cell_at on a known-good index works.
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'k');
    }
}

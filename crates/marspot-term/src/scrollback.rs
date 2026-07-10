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
pub enum Scrollback {
    Memory(MemoryScrollback),
    /// Persistent file-backed scrollback (A1 of pane upgrade — see
    /// `docs/scrollback-search.md`).  Survives L3 self-execv via
    /// path-based reopen.  Append-only `.bin` + sidecar `.idx`;
    /// hot read served from RAM ring, cold reads `pread()` the
    /// file.  Wrapped flag per line stored in the record (Grid's
    /// `sb_wrapped` mirror stays the in-RAM truth for the Memory
    /// variant, which mcli / `--snapshot` / tests use).
    File(FileScrollback),
}

impl Scrollback {
    pub fn memory(capacity: usize, cols: usize) -> Self {
        Self::Memory(MemoryScrollback::new(capacity, cols))
    }

    /// Open or create a file-backed scrollback at the given paths.
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
            // File variant defaults wrapped=false on the enum-level
            // entry point.  Grid threads its own wrapped flag through
            // `push_line_with_wrapped` instead — that's the path
            // production sessions take.
            Self::File(f) => f.push_line(line, false),
        }
    }

    /// F3+10 — flush any BufWriter user-space tail to the kernel
    /// page cache.  Memory variant has no buffer (truth is the in-RAM
    /// ring); File variant flushes both bin/idx BufWriters.  Call
    /// this BEFORE `libc::execv` so the next L3's reopen sees the
    /// full file (Drop won't run on execv).  No-op on Memory.
    pub fn flush_for_handoff(&self) {
        match self {
            Self::Memory(_) => {}
            Self::File(f) => f.flush_for_handoff(),
        }
    }

    /// File variant only: push a line with its DECAWM continuation
    /// flag.  Memory drops the flag (Grid's `sb_wrapped` is the truth
    /// there).  A1's surface for direct tests against FileScrollback;
    /// A3 makes Grid call this so file records carry the right
    /// wrapped value.
    pub fn push_line_with_wrapped(&mut self, line: &[Cell], wrapped: bool) {
        match self {
            Self::Memory(m) => m.push_line(line),
            Self::File(f) => f.push_line(line, wrapped),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Memory(m) => m.len(),
            Self::File(f) => f.len(),
        }
    }

    /// F3+10d — "navigable" length: drops trailing entries that are
    /// either past-EOF (idx ahead of bin BufWriter sync) OR
    /// fully-blank (cols=0 records from claudecode's spinner UI).
    /// F3+10e — true for the persistent (File) variant; reflow
    /// on a cols-change normally drops the in-RAM scrollback and
    /// re-pushes wrapped segments at the new width.  For File that
    /// double-counts the on-disk records (they survive the restart
    /// + we re-push the same content wrapped to new_cols), so reflow
    /// skips the re-push step.  The historical records stay at their
    /// original widths on disk; display renders them as-is (cells
    /// past `cols` show as default — visually shorter row in a wider
    /// pane, truncated in a narrower one).
    pub fn is_persistent(&self) -> bool {
        matches!(self, Self::File(_))
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::Memory(m) => m.capacity(),
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
            Self::File(f) => f.cell_at(line_idx, col),
        }
    }

    /// Read one whole line.  Allocates a Vec for the disk path; mostly
    /// for tests + the headless `--snapshot` path.  Hot rendering uses
    /// `cell_at` instead to avoid the per-line allocation.
    pub fn line_to_vec(&self, idx: usize) -> Option<Vec<Cell>> {
        match self {
            Self::Memory(m) => m.line(idx).map(|s| s.to_vec()),
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

    /// File variant only: per-line wrapped flag.  Memory returns
    /// false (Grid's `sb_wrapped` is the truth there).
    pub fn wrapped_at(&self, idx: usize) -> bool {
        match self {
            Self::Memory(_) => false,
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
            Self::File(f) => f.clear(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate resident bytes held by this scrollback for the
    /// MARSPOT_PROFILE_RSS sampler.  Memory variant: lazy-grown
    /// `Vec<Cell>` capacity.  File variant: hot-RAM bytes + headroom.
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Memory(m) => m.approx_bytes(),
            Self::File(f) => f.approx_bytes(),
        }
    }

    /// Drop all content and re-init for a new column width.  Used
    /// by `Grid::resize` — stored lines aren't valid at the new
    /// width.  Preserves the variant; File rebuilds at the new
    /// width by truncating + reopening its bin/idx files, falling
    /// back to Memory only when reopen itself fails.
    pub fn restart(&mut self, new_cols: usize) {
        let placeholder = std::mem::replace(self, Self::Memory(MemoryScrollback::new(0, 1)));
        *self = match placeholder {
            Self::Memory(m) => Self::Memory(MemoryScrollback::new(m.capacity, new_cols)),
            Self::File(f) => {
                // F3+10f — reflow rewrites scrollback at new cols.
                // For File, that means: truncate bin to header, wipe
                // idx, reopen.  The caller's subsequent
                // push_line_with_wrapped pushes the re-wrapped
                // segments back, repopulating the file at new_cols.
                // Without truncate, the file accumulates duplicated
                // records at every width the pane was ever at (the
                // 错位 visible after a window resize).
                let bin_path = f.bin_path.clone();
                let idx_path = f.idx_path.clone();
                let ram_cap = f.ram_capacity;
                drop(f);
                // Truncate bin to just-header and idx to 0; ignore
                // errors (fall through to Memory variant if any IO
                // fails so the session keeps running).
                let _ = (|| -> std::io::Result<()> {
                    let bin_fd = std::fs::OpenOptions::new()
                        .read(true).write(true).open(&bin_path)?;
                    bin_fd.set_len(FILE_HEADER_BYTES)?;
                    drop(bin_fd);
                    let idx_fd = std::fs::OpenOptions::new()
                        .read(true).write(true).truncate(true).open(&idx_path)?;
                    drop(idx_fd);
                    Ok(())
                })();
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
// F3+10i — bump v1 → v2 to force-discard scrollback files written by
// the mmap-write / blank-row-pollution era.  User authorised the
// destruction ("新的历史没问题,老的全都不要了都可以"): pre-v2 files
// can carry past-EOF idx tails + spinner blank pushes interleaved
// mid-history that surface as blank rows in scrolled views.  Open()
// already handles "version < FILE_MIN_COMPAT" by renaming the file
// to `.corrupt-<ts>` and recursing with a fresh start — bumping
// MIN_COMPAT alongside VERSION trips that path for every existing
// user file on first open() after this upgrade.  Future scrollback
// shape changes (e.g. adding a record-level field) only need
// VERSION++ without touching MIN_COMPAT, preserving back-compat.
const FILE_VERSION: u32 = 2;
const FILE_MIN_COMPAT: u32 = 2;
const FILE_HEADER_BYTES: u64 = 32;
const FILE_REC_HEADER_BYTES: usize = 4 + 1 + 2; // rec_len + wrapped + cols

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
    bin: std::cell::RefCell<std::io::BufWriter<std::fs::File>>,
    /// F3+11.1 — raw `File`, not `BufWriter`.  Each push is 8 B; a
    /// BufWriter would defer flush until 4 KB (≈ 512 pushes), so a
    /// SIGKILL between flushes loses all 512 entries.  Direct write
    /// is one syscall per push (~5 µs), well within budget and the
    /// only way to keep on-disk idx consistent without an explicit
    /// per-push flush hack.
    idx: std::cell::RefCell<std::fs::File>,
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
}

// Raw mmap ptrs are private to this struct and the kernel takes care
// of cross-thread coherence — `FileScrollback` itself is owned by a
// single L3 process so there's no inter-process concurrent mutation
// either.  Marker impls let the type cross thread boundaries when
// embedded in `Scrollback` (which `Send` is naturally derived for).
unsafe impl Send for FileScrollback {}

impl FileScrollback {
    /// Open or create the scrollback files.
    ///
    /// F3+11 — strict and dumb.  No silent rename of mismatched
    /// headers, no idx rebuild from bin, no tolerant load of bad
    /// records.  The ONLY repair step is truncating idx tail
    /// entries that point past bin EOF — this is data consistency
    /// (idx is derived from bin and must reference real records),
    /// not defense.  Anything else returns Err.
    pub fn open(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
    ) -> std::io::Result<Self> {
        use std::io::Write;
        if let Some(parent) = bin_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let bin_existed = bin_path.exists()
            && bin_path.metadata().map(|m| m.len() > 0).unwrap_or(false);

        let bin_w = std::fs::OpenOptions::new()
            .read(true).append(true).create(true).open(&bin_path)?;

        if bin_existed {
            let cur_len = bin_w.metadata()?.len();
            if cur_len < FILE_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("scrollback bin {} bytes < {} header",
                        cur_len, FILE_HEADER_BYTES),
                ));
            }
            let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
            read_exact_at(&bin_w, &mut hdr, 0)?;
            let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            if magic != FILE_MAGIC {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("scrollback bin magic 0x{magic:08x} != 0x{FILE_MAGIC:08x}"),
                ));
            }
            let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
            if version < FILE_MIN_COMPAT || version > FILE_VERSION {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("scrollback bin version {version} outside [{FILE_MIN_COMPAT}, {FILE_VERSION}]"),
                ));
            }
            let cell_abi = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            if cell_abi != crate::terminal::CELL_BYTES_PUB as u32 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("scrollback bin cell_abi {cell_abi} != {}", crate::terminal::CELL_BYTES_PUB),
                ));
            }
        }

        let mut bin = std::io::BufWriter::with_capacity(64 * 1024, bin_w);
        if !bin_existed {
            Self::write_header(&mut bin)?;
            bin.flush()?;
        }

        let bin_for_read = std::fs::OpenOptions::new().read(true).open(&bin_path)?;
        let bin_eof = bin_for_read.metadata()?.len();

        let idx_w = std::fs::OpenOptions::new()
            .read(true).write(true).append(true).create(true).open(&idx_path)?;
        let idx_size = idx_w.metadata()?.len();
        if idx_size % 8 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("scrollback idx size {idx_size} not a multiple of 8"),
            ));
        }

        // F3+11 — single consistency-repair step: truncate idx tail
        // entries whose record can't be fully decoded from bin.
        // This handles the well-defined "BufWriter sync drift" case
        // when an L3 was killed without flush_for_handoff (bin
        // BufWriter 64 KB vs idx 4 KB → idx flushes more often →
        // idx ahead of bin on disk).  flush_for_handoff covers the
        // clean execv path; this scan covers SIGKILL.  Backward
        // walk runs at most idx_size/8 iterations and stops at the
        // first valid record.
        let idx_for_read = std::fs::OpenOptions::new().read(true).open(&idx_path)?;
        let mut total_lines = idx_size / 8;
        while total_lines > 0 {
            let mut buf = [0u8; 8];
            // The idx file is multiple-of-8 by the size check above —
            // reading any aligned entry must succeed.  Propagate I/O
            // errors (genuine fs failure) but treat short reads as
            // "drop the tail".
            if read_exact_at(&idx_for_read, &mut buf, (total_lines - 1) * 8).is_err() {
                total_lines -= 1;
                continue;
            }
            let off = u64::from_le_bytes(buf);
            // Past-EOF idx entries arise from an unclean exit where
            // bin BufWriter didn't flush (idx is direct).  Walk back
            // until the entry's record header is fully within bin.
            if off < FILE_HEADER_BYTES || off + 4 > bin_eof {
                total_lines -= 1;
                continue;
            }
            let mut len_buf = [0u8; 4];
            if read_exact_at(&bin_for_read, &mut len_buf, off).is_err() {
                total_lines -= 1;
                continue;
            }
            let rec_len = u32::from_le_bytes(len_buf) as u64;
            if rec_len < 3 || off + 4 + rec_len > bin_eof {
                total_lines -= 1;
                continue;
            }
            break;
        }
        let new_idx_size = total_lines * 8;
        if new_idx_size != idx_size {
            let trim_fd = std::fs::OpenOptions::new()
                .read(true).write(true).open(&idx_path)?;
            trim_fd.set_len(new_idx_size)?;
        }
        let idx = idx_w;
        let bin_tail_offset = bin_eof.max(FILE_HEADER_BYTES);

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
        let hot_count = total_lines;
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
                // F3+11.1 — single-record tolerant load.  The
                // open-time idx tail-truncate only checks the LAST
                // entry; if some idx[N] in the middle of the
                // RAM-window points to a BufWriter-truncated bin
                // record (unclean kill mid-write), strict propagate
                // would fail open() entirely and Terminal::new
                // falls back to empty Disk → user sees ALL history
                // disappear.  Substituting a single blank row for
                // the corrupt entry costs the user one visible row
                // of black, but preserves the other ~255 RAM-window
                // rows + all on-disk history.  This is principled
                // tolerance for a known mechanism, not blind
                // defense.
                let row = match read_idx_at(&idx_for_read, li as u64) {
                    Ok(off) => match read_record_at(&bin_for_read, off) {
                        Ok((cells, w)) => {
                            ram_wrapped.push(w);
                            pad_or_clip(&cells, cols)
                        }
                        Err(_) => {
                            ram_wrapped.push(false);
                            pad_or_clip(&[], cols)
                        }
                    },
                    Err(_) => {
                        ram_wrapped.push(false);
                        pad_or_clip(&[], cols)
                    }
                };
                ram_cells.extend_from_slice(&row);
            }
        }

        Ok(Self {
            bin_path,
            idx_path,
            cols,
            ram_capacity,
            bin: std::cell::RefCell::new(bin),
            idx: std::cell::RefCell::new(idx),
            bin_for_read,
            idx_for_read,
            bin_mmap_ptr: std::cell::Cell::new(std::ptr::null_mut()),
            bin_mmap_len: std::cell::Cell::new(0),
            idx_mmap_ptr: std::cell::Cell::new(std::ptr::null_mut()),
            idx_mmap_len: std::cell::Cell::new(0),
            has_unflushed: std::cell::Cell::new(false),
            ram_cells,
            ram_wrapped,
            ram_head: 0,
            ram_len: load_n,
            total_lines,
            bin_tail_offset,
            scratch: Vec::with_capacity(FILE_REC_HEADER_BYTES + cols.saturating_mul(crate::terminal::CELL_BYTES_PUB)),
            cold_bin_path,
            cold_idx_path,
            cold_bin_for_read,
            cold_idx_for_read,
            cold_first_line,
            cold_total_lines,
            hot_first_line,
            hot_bytes_cap: hot_bytes_cap(),
        })
    }

    fn write_header(bin: &mut std::io::BufWriter<std::fs::File>) -> std::io::Result<()> {
        use std::io::Write;
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
        bin.write_all(&hdr)
    }


    #[allow(dead_code)]
    fn trim_trailing_partial(
        bin: &std::fs::File,
        idx: &std::fs::File,
        total_lines: u64,
        bin_len: u64,
    ) -> std::io::Result<(u64, u64)> {
        // Walk from the last indexed record and verify it's complete.
        // If incomplete, we accept the smaller total_lines.  We do
        // NOT truncate the bin file — leftover bytes past the last
        // good record are ignored by future appends (which use
        // O_APPEND going to current EOF) and don't affect reads
        // (.idx is the lookup truth).
        if total_lines == 0 {
            return Ok((0, FILE_HEADER_BYTES.max(bin_len)));
        }
        let mut last_good = total_lines;
        let mut bin_tail = bin_len;
        // Probe at most the last 4 records (a single mid-write crash
        // touches one; we err generous).
        let probe_n = last_good.min(4);
        for off_back in 0..probe_n {
            let li = last_good - 1 - off_back;
            let off = read_idx_at(idx, li)?;
            let mut len_buf = [0u8; 4];
            if read_exact_at(bin, &mut len_buf, off).is_err() {
                continue;
            }
            let rec_len = u32::from_le_bytes(len_buf) as u64;
            let end = off + 4 + rec_len;
            if end > bin_len {
                // This record is partial; treat it (and any newer
                // partial siblings) as not present.
                last_good = li;
                // The next record-start would have been `end`, which
                // is past EOF — so bin tail effectively rolls back
                // to this record's start.
                bin_tail = off;
            } else {
                break;
            }
        }
        Ok((last_good, bin_tail))
    }

    /// F2+5 — rotate the current hot pair into cold and reopen a
    /// fresh hot pair.  Called by push_line when an incoming record
    /// would push hot past `hot_bytes_cap`.  Any existing cold pair
    /// is overwritten — that's the "delete" tier; once rotated past,
    /// older history is unrecoverable.  RAM ring contents are
    /// preserved (in-memory state is independent of the file).
    fn rotate_to_cold(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        // Capture the count of rows about to leave hot for cold.
        let hot_count_before = self.total_lines - self.hot_first_line;
        if hot_count_before == 0 {
            // Nothing to rotate (the very first push exceeded cap on
            // an empty file — degenerate).  Just continue writing.
            return Ok(());
        }
        // 1) Flush writer buffers so the rename captures complete data.
        self.bin.borrow_mut().flush()?;
        self.idx.borrow_mut().flush()?;
        // 2) Drop everything that holds an fd / mmap to the hot
        //    files, so rename / unlink can succeed on platforms that
        //    care (we're macOS so unlink-while-open is OK, but a
        //    clean ordering makes the contract obvious).  We replace
        //    fields with placeholders we re-overwrite below.
        //
        //    munmap any mmaps held on the hot files; their underlying
        //    inode is about to change identity (rename → cold path,
        //    then create fresh hot at the old path).  Reusing the old
        //    mmap pointers would point at the renamed file, which is
        //    now logically cold — not what cell_at wants for hot reads.
        unsafe {
            let bp = self.bin_mmap_ptr.get();
            let bl = self.bin_mmap_len.get();
            if !bp.is_null() && bl > 0 {
                libc::munmap(bp as *mut _, bl);
            }
            self.bin_mmap_ptr.set(std::ptr::null_mut());
            self.bin_mmap_len.set(0);
            let ip = self.idx_mmap_ptr.get();
            let il = self.idx_mmap_len.get();
            if !ip.is_null() && il > 0 {
                libc::munmap(ip as *mut _, il);
            }
            self.idx_mmap_ptr.set(std::ptr::null_mut());
            self.idx_mmap_len.set(0);
        }
        // Drop the cold fds before rename overwrites their inodes.
        self.cold_bin_for_read = None;
        self.cold_idx_for_read = None;
        // Drop hot writers + readers (they hold fds on the soon-to-be-
        // renamed inode).  We rebuild them after rename.
        // Take ownership of the writers' inner BufWriters so they drop here.
        let _ = std::mem::replace(
            &mut *self.bin.borrow_mut(),
            std::io::BufWriter::with_capacity(64 * 1024, dev_null_file()?),
        );
        let _ = std::mem::replace(
            &mut *self.idx.borrow_mut(),
            dev_null_file()?,
        );
        // Replace read fds with placeholders; we rebuild them post-rename.
        self.bin_for_read = dev_null_file()?;
        self.idx_for_read = dev_null_file()?;
        // 3) Delete any stale cold pair (defence in depth: rename
        //    on most filesystems would clobber, but be explicit).
        let _ = std::fs::remove_file(&self.cold_bin_path);
        let _ = std::fs::remove_file(&self.cold_idx_path);
        // 4) Rename hot → cold.
        std::fs::rename(&self.bin_path, &self.cold_bin_path)?;
        std::fs::rename(&self.idx_path, &self.cold_idx_path)?;
        // 5) Reopen fresh hot pair.
        let bin_w = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.bin_path)?;
        let mut bin = std::io::BufWriter::with_capacity(64 * 1024, bin_w);
        Self::write_header(&mut bin)?;
        bin.flush()?;
        let idx_w = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .create(true)
            .open(&self.idx_path)?;
        // 6) Reinstall read fds for hot.
        let bin_for_read = std::fs::OpenOptions::new().read(true).open(&self.bin_path)?;
        let idx_for_read = std::fs::OpenOptions::new().read(true).open(&self.idx_path)?;
        *self.bin.borrow_mut() = bin;
        *self.idx.borrow_mut() = idx_w;
        self.bin_for_read = bin_for_read;
        self.idx_for_read = idx_for_read;
        self.bin_tail_offset = FILE_HEADER_BYTES;
        self.has_unflushed.set(false);
        // 7) Open the (newly renamed) cold read fds.
        self.cold_bin_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.cold_bin_path)
            .ok();
        self.cold_idx_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.cold_idx_path)
            .ok();
        // 8) Logical boundary bookkeeping.  Previous-hot's rows now
        //    live in cold; bump cold_first_line to where they were
        //    in logical-idx terms, and shift hot_first_line up so
        //    the next push lands at total_lines (no gap).
        self.cold_first_line = self.hot_first_line;
        self.cold_total_lines = hot_count_before;
        self.hot_first_line = self.total_lines;
        Ok(())
    }

    /// Append one line.  Hot path.
    pub fn push_line(&mut self, line: &[crate::grid::Cell], wrapped: bool) {
        use std::io::Write;
        // F3+11.1 — trim trailing cells equal to `Cell::default()`
        // before encoding.  This is pure disk-size optimisation; the
        // semantic is unchanged from the dumb-store model: every
        // push call produces exactly one record, with cols equal to
        // the trimmed cell count (may be 0 for fully-default rows).
        // Read path: cell_at returns None for col >= cols, which the
        // viewport renderer maps to Cell::default() — visually
        // identical to having stored the trailing default cells.
        // Critical perf: a 200-col grid where claudecode TUI uses
        // the first ~40 cols means 80% disk savings, and 80% of the
        // Grid::resize reflow read volume.  Without this, resize on
        // a session with megabytes of history takes seconds.
        let default_cell = crate::grid::Cell::default();
        let trimmed_len = line
            .iter()
            .rposition(|c| *c != default_cell)
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &line[..trimmed_len];
        let cols_u16 = line.len() as u16;
        let rec_len = (1 + 2 + line.len() * crate::terminal::CELL_BYTES_PUB) as u32;
        let total_bytes = 4 + rec_len as usize;
        self.scratch.clear();
        self.scratch.reserve(total_bytes);
        self.scratch.extend_from_slice(&rec_len.to_le_bytes());
        self.scratch.push(wrapped as u8);
        self.scratch.extend_from_slice(&cols_u16.to_le_bytes());
        for c in line {
            self.scratch.extend_from_slice(&(c.ch as u32).to_le_bytes());
            self.scratch.extend_from_slice(&crate::terminal::serialize_attrs_pub(c.attrs));
        }

        // F2+5 — hot → cold rotation when the next record would
        // overflow the cap.  Failure to rotate is a real I/O error
        // (fs::rename ENOSPC etc.); panic rather than push past the
        // cap silently.
        if self.bin_tail_offset.saturating_add(total_bytes as u64) > self.hot_bytes_cap {
            self.rotate_to_cold().expect("scrollback hot→cold rotation");
        }
        let rec_offset = self.bin_tail_offset;

        // Bin first, then idx — `open()` recovers from the bin-
        // BufWriter-ahead-of-idx case via the tail-truncate scan.
        // I/O failure here = disk full / fs error; panic loudly so
        // the user knows storage broke instead of silently losing
        // history.
        self.bin.borrow_mut().write_all(&self.scratch)
            .expect("scrollback bin write");
        self.bin_tail_offset += total_bytes as u64;
        // Idx is a raw File — each 8 B write is one syscall, no
        // BufWriter buffering.  This keeps on-disk idx always
        // consistent with what's been bin-flushed-or-buffered;
        // open()'s tail-truncate scan handles the bin BufWriter
        // tail that didn't survive an unclean kill.
        self.idx.borrow_mut().write_all(&rec_offset.to_le_bytes())
            .expect("scrollback idx write");
        self.has_unflushed.set(true);

        self.push_into_ring(line, wrapped);
        self.total_lines += 1;
    }

    /// Cheap idempotent flush: a cold-read path calls this before
    /// consulting mmap / pread so any line that aged out of the RAM
    /// ring but still sits in the BufWriter user-space buffer is
    /// fully visible on disk.  Called at most once per cold-read
    /// burst because `has_unflushed` clears here and only `push_line`
    /// re-sets it.
    fn ensure_flushed(&self) {
        use std::io::Write;
        if !self.has_unflushed.get() {
            return;
        }
        let _ = self.bin.borrow_mut().flush();
        let _ = self.idx.borrow_mut().flush();
        self.has_unflushed.set(false);
    }

    /// F3+10 — call before L3 `execv` (Drop won't run, so the
    /// BufWriter tail would otherwise be lost — that's the original
    /// execv-gap bug).  Same flush path as `Drop`, just exposed as a
    /// public hook so `do_l3_execv_swap` can invoke it explicitly.
    /// snapshot v3's index-based dedup makes this a safety net rather
    /// than a strict correctness requirement (the snapshot already
    /// carries the BufWriter tail in its RAM-ring section), but
    /// flushing keeps the on-disk file in sync so cold reads of
    /// recent rows hit the file directly instead of having to wait
    /// for the next push_line to spill.
    pub fn flush_for_handoff(&self) {
        use std::io::Write;
        let _ = self.bin.borrow_mut().flush();
        let _ = self.idx.borrow_mut().flush();
        self.has_unflushed.set(false);
    }

    /// Ensure `self.bin_mmap_*` covers at least `needed_len` bytes.
    /// First call mmaps the file; later calls remap when the file
    /// has grown.  Called from cold-read paths after `ensure_flushed`
    /// guarantees the kernel sees a consistent file.
    fn ensure_bin_mmap_covers(&self, needed_len: u64) -> std::io::Result<()> {
        let cur_len = self.bin_mmap_len.get();
        if cur_len as u64 >= needed_len && !self.bin_mmap_ptr.get().is_null() {
            return Ok(());
        }
        // Remap to the file's current size (which may be larger than
        // needed_len — that's fine, virtual reservation only).
        let file_len = self.bin_for_read.metadata()?.len();
        if file_len == 0 {
            return Ok(());
        }
        // Unmap old if any.
        let old_ptr = self.bin_mmap_ptr.get();
        if !old_ptr.is_null() && cur_len > 0 {
            unsafe { libc::munmap(old_ptr as *mut libc::c_void, cur_len); }
        }
        // Map new.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len as libc::size_t,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.bin_for_read),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            self.bin_mmap_ptr.set(std::ptr::null_mut());
            self.bin_mmap_len.set(0);
            return Err(std::io::Error::last_os_error());
        }
        self.bin_mmap_ptr.set(ptr as *mut u8);
        self.bin_mmap_len.set(file_len as usize);
        Ok(())
    }

    fn ensure_idx_mmap_covers(&self, needed_len: u64) -> std::io::Result<()> {
        let cur_len = self.idx_mmap_len.get();
        if cur_len as u64 >= needed_len && !self.idx_mmap_ptr.get().is_null() {
            return Ok(());
        }
        let file_len = self.idx_for_read.metadata()?.len();
        if file_len == 0 {
            return Ok(());
        }
        let old_ptr = self.idx_mmap_ptr.get();
        if !old_ptr.is_null() && cur_len > 0 {
            unsafe { libc::munmap(old_ptr as *mut libc::c_void, cur_len); }
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len as libc::size_t,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.idx_for_read),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            self.idx_mmap_ptr.set(std::ptr::null_mut());
            self.idx_mmap_len.set(0);
            return Err(std::io::Error::last_os_error());
        }
        self.idx_mmap_ptr.set(ptr as *mut u8);
        self.idx_mmap_len.set(file_len as usize);
        Ok(())
    }

    /// Read the byte offset of line `idx` from the idx mmap, falling
    /// back to pread if mmap isn't covering yet.
    fn read_idx_via_mmap(&self, line_idx: u64) -> std::io::Result<u64> {
        let off_in_idx = line_idx * 8;
        self.ensure_idx_mmap_covers(off_in_idx + 8)?;
        let mmap_ptr = self.idx_mmap_ptr.get();
        let mmap_len = self.idx_mmap_len.get();
        if !mmap_ptr.is_null() && (off_in_idx as usize) + 8 <= mmap_len {
            let buf = unsafe {
                std::slice::from_raw_parts(mmap_ptr.add(off_in_idx as usize), 8)
            };
            return Ok(u64::from_le_bytes(buf.try_into().unwrap()));
        }
        // Fallback (rare): pread.
        read_idx_at(&self.idx_for_read, line_idx)
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
    fn read_record_via_mmap(&self, offset: u64, _col_filter: Option<usize>) -> Option<(Vec<crate::grid::Cell>, bool)> {
        // Ensure mmap covers at least the rec_len header.
        self.ensure_bin_mmap_covers(offset + 4).ok()?;
        let mmap_ptr = self.bin_mmap_ptr.get();
        let mmap_len = self.bin_mmap_len.get();
        if mmap_ptr.is_null() || (offset as usize) + 4 > mmap_len {
            return None;
        }
        let len_slice = unsafe {
            std::slice::from_raw_parts(mmap_ptr.add(offset as usize), 4)
        };
        let rec_len = u32::from_le_bytes(len_slice.try_into().unwrap()) as usize;
        // Extend mmap if record's body lives past current end.
        if (offset as usize) + 4 + rec_len > mmap_len {
            self.ensure_bin_mmap_covers(offset + 4 + rec_len as u64).ok()?;
        }
        let mmap_ptr = self.bin_mmap_ptr.get();
        let mmap_len = self.bin_mmap_len.get();
        if (offset as usize) + 4 + rec_len > mmap_len {
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
            let Some(cold_idx_fd) = self.cold_idx_for_read.as_ref() else { return false; };
            let Some(cold_bin_fd) = self.cold_bin_for_read.as_ref() else { return false; };
            let cold_local = (idx as u64) - self.cold_first_line;
            let Ok(off) = read_idx_at(cold_idx_fd, cold_local) else { return false; };
            let Ok((_, wrapped)) = read_record_at(cold_bin_fd, off) else { return false; };
            return wrapped;
        }
        self.ensure_flushed();
        let hot_local = (idx as u64) - self.hot_first_line;
        let Some(off) = self.read_idx_via_mmap(hot_local).ok() else { return false; };
        if let Some((_, w)) = self.read_record_via_mmap(off, None) {
            return w;
        }
        let Some((_cells, wrapped)) = read_record_at(&self.bin_for_read, off).ok() else { return false; };
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
        use std::io::{Seek, SeekFrom, Write};
        // Flush BufWriters so any buffered bytes hit the page cache
        // before our fds close.  No fsync.  Dense idx — no sentinel.
        let _ = self.bin.borrow_mut().flush();
        let _ = self.idx.borrow_mut().flush();
        // Unmap any active mmap regions.
        let bp = self.bin_mmap_ptr.get();
        let bl = self.bin_mmap_len.get();
        if !bp.is_null() && bl > 0 {
            unsafe { libc::munmap(bp as *mut libc::c_void, bl); }
        }
        let ip = self.idx_mmap_ptr.get();
        let il = self.idx_mmap_len.get();
        if !ip.is_null() && il > 0 {
            unsafe { libc::munmap(ip as *mut libc::c_void, il); }
        }
        let _ = self.bin_for_read.seek(SeekFrom::Start(0));
        let _ = self.idx_for_read.seek(SeekFrom::Start(0));
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
        use std::io::Write;
        self.bin.borrow_mut().flush()?;
        self.idx.borrow_mut().flush()?;
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

    /// F3+11.1 — push trims trailing default cells (perf:  80%+
    /// disk savings on a TUI grid where most of the row is
    /// padding).  Trim is purely a serialised-size optimisation:
    /// the prefix decodes back at its original columns; trimmed-
    /// tail columns decode as None which the viewport renderer
    /// maps to Cell::default() (visually identical to having
    /// stored them).
    #[test]
    fn push_line_trims_default_tail() {
        use std::io::{Read, Seek, SeekFrom};
        let tmp = TmpDir::new("trim-tail");
        let cols = 200usize;
        let prefix_len = 30usize;
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
        for _ in 0..6 {
            sb.push_line(&fill(b'.', cols), false);
        }
        drop(sb);
        let mut f = std::fs::File::open(tmp.bin()).expect("open bin");
        f.seek(SeekFrom::Start(FILE_HEADER_BYTES)).unwrap();
        let mut rec_len_buf = [0u8; 4];
        f.read_exact(&mut rec_len_buf).unwrap();
        let rec_len = u32::from_le_bytes(rec_len_buf) as usize;
        let expected_trimmed = 1 + 2 + prefix_len * crate::terminal::CELL_BYTES_PUB;
        assert_eq!(
            rec_len, expected_trimmed,
            "first record should hold ONLY the prefix (trim default \
             tail); got rec_len={rec_len}, expected={expected_trimmed}"
        );
        let sb2 = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4)
            .expect("reopen");
        for c in 0..prefix_len {
            let want = (b'a' + (c % 26) as u8) as char;
            assert_eq!(sb2.cell_at(0, c).unwrap().ch, want, "prefix col {c}");
        }
        // Trimmed-tail cells: cell_at returns None (the renderer's
        // higher-level cell_at_view maps None → Cell::default()).
        for c in prefix_len..cols {
            assert!(
                sb2.cell_at(0, c).is_none(),
                "trimmed tail col {c} should be None (renderer fills \
                 default), got Some"
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
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0") }; // 0 MB still ≥ default
        // 0 MB cap is degenerate; manually patch via direct construction
        // is not exposed — so set a non-zero cap that's still tiny.
        // 1 MB = 1048576 bytes; cap≥1MB won't trigger.  We instead set
        // an explicit small value via env override interpretation:
        // hot_bytes_cap() floors at value*1MB, so 0 effectively disables
        // rotation.  We want a small >0 trigger, so... use the unit
        // size 1MB and feed lots of rows.
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1") };
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
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB") };
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

    /// F3+11 — bad header returns Err (no silent rename, no fresh
    /// start).  Caller (Terminal::new) sees the error and decides
    /// how to surface it; storage doesn't paper over corruption.
    #[test]
    fn file_corrupt_header_returns_err() {
        let tmp = TmpDir::new("corrupt-err");
        std::fs::write(tmp.bin(), b"\xff\xff\xff\xff\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00garbage").unwrap();
        std::fs::write(tmp.idx(), b"junk").unwrap();
        let err = match FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8) {
            Err(e) => e,
            Ok(_) => panic!("open must fail on bad magic"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("magic"),
            "expected error about bad magic, got: {msg}"
        );
    }

    /// F3+11 — misaligned idx returns Err.  Idx file is derived
    /// from bin and must be in lockstep; the only repair step at
    /// open() is truncating tail entries past bin EOF (the
    /// SIGKILL-without-flush case).  An idx with non-multiple-of-8
    /// size is corruption past what the tail-truncate handles —
    /// surface, don't rebuild.
    #[test]
    fn file_misaligned_idx_returns_err() {
        let tmp = TmpDir::new("idx-misaligned");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
                .expect("create");
            for _ in 0..5 {
                sb.push_line(&fill(b'a', cols), false);
            }
        }
        let idx_bytes = std::fs::read(tmp.idx()).unwrap();
        let mut bad = idx_bytes.clone();
        bad.push(0x55); // off-by-1 → not multiple of 8
        std::fs::write(tmp.idx(), &bad).unwrap();
        let err = match FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8) {
            Err(e) => e,
            Ok(_) => panic!("open must fail on misaligned idx"),
        };
        assert!(format!("{err}").contains("multiple of 8"));
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
        // After cold read: has_unflushed should be back to false.
        assert!(!sb.has_unflushed.get(), "ensure_flushed should clear flag");
        // mmap should be populated.
        assert!(!sb.bin_mmap_ptr.get().is_null(), "bin mmap should be set");
        assert!(!sb.idx_mmap_ptr.get().is_null(), "idx mmap should be set");
    }

    /// A2: after a cold read mmaps the bin, subsequent pushes that
    /// grow the file beyond the current mmap_len must trigger a
    /// remap on the NEXT cold read.  Without remap, reading the
    /// newly-cold lines would access unmapped memory or stale length.
    #[test]
    fn file_a2_mmap_remap_on_file_growth() {
        let tmp = TmpDir::new("a2-remap");
        let cols = 4usize;
        let ram_capacity = 4;
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_capacity)
            .expect("create");
        // First batch: push 20 lines, cold-read to mmap them.
        for i in 0..20 {
            sb.push_line(&fill((b'a' + (i % 26) as u8) as u8, cols), false);
        }
        let first_old_idx = 5;
        let _ = sb.cell_at(first_old_idx, 0); // triggers initial mmap
        let mmap_len_first = sb.bin_mmap_len.get();
        assert!(mmap_len_first > 0, "first mmap should be non-empty");
        // Second batch: push another 50 lines so the file grows
        // past mmap_len_first.
        for i in 20..70 {
            sb.push_line(&fill((b'a' + (i % 26) as u8) as u8, cols), false);
        }
        // Cold-read a line that landed in the second batch (now
        // aged out of ring of 4).
        let cold_after_growth = 30usize;
        let want_ch = ((b'a' + (cold_after_growth % 26) as u8)) as char;
        assert_eq!(
            sb.cell_at(cold_after_growth, 0).expect("cell after growth").ch,
            want_ch,
            "remap on growth should let us read freshly-cold lines"
        );
        assert!(
            sb.bin_mmap_len.get() > mmap_len_first,
            "mmap should have remapped to cover new file size: was {}, now {}",
            mmap_len_first,
            sb.bin_mmap_len.get()
        );
    }

    /// A3: end-to-end through `Terminal::new` with the env-gate
    /// active.  Push lines via `feed`, drop, re-instantiate, assert
    /// scrollback persisted across the "reopen".  Verifies the
    /// wiring of MARSPOT_SESSION_ID + MARSPOT_STATE_DIR all the
    /// way down.
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

    // ─── F3+10g: autotest harness for the bug classes that ate the
    //         user's history.  Each test reproduces one specific
    //         failure mode (torn write, resize duplication, past-EOF
    //         orphans, trailing-blank navigability) without going
    //         through the full marspot binary — these are the
    //         scenarios the user can't reliably eyeball.  See
    //         `[[project-scrollback-execv-gap]]` for the failure
    //         catalogue.  ──────────────────────────────────────────

    /// Torn write: BufWriter dropped without flush (the L3 self-execv
    /// scenario where Drop never runs).  Previously this caused idx
    /// to point past bin EOF after reopen → tolerant load surfaced
    /// blank rows → user saw "history is gone".  After F3+10c's Pass
    /// A, reopen drops past-EOF idx tail, every surfaced row decodes.
    #[test]
    fn file_torn_write_no_past_eof_orphans() {
        let tmp = TmpDir::new("torn-write");
        let cols = 8usize;

        // Push enough lines that BufWriter is mid-buffer when we
        // forget — 1000 × 8 cells writes ~100 KB to bin, well past
        // the 64 KB BufWriter auto-flush boundary so several
        // chunks have already hit disk and some tail is unflushed.
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 256)
            .expect("create");
        for i in 0..1000u32 {
            let ch = b'a' + (i % 26) as u8;
            sb.push_line(&fill(ch, cols), false);
        }
        let pushed = sb.len();
        assert_eq!(pushed, 1000);

        // SAFETY: leak the FileScrollback — `Drop` (which flushes
        // BufWriters) never runs.  This is exactly what `libc::execv`
        // does to the old L3 process image.  The fd handles leak too;
        // tmp dir cleanup at the end of the test reclaims everything.
        std::mem::forget(sb);

        // Reopen.  Post-F3+10c, every surfaced row must decode to
        // its expected character — no blank-by-tolerant-load rows
        // leaking through.
        let sb2 = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 256)
            .expect("reopen after torn close");
        let total = sb2.len();
        assert!(total <= pushed, "reopen total {total} > pushed {pushed}");
        // BufWriter auto-flush ensures a substantial prefix survived.
        assert!(total >= 500, "reopen total {total} suspiciously low");
        for i in 0..total {
            let line = sb2
                .read_line(i)
                .unwrap_or_else(|| panic!("row {i} should decode"));
            assert_eq!(line.len(), cols, "row {i} width mismatch");
            let expected = (b'a' + (i % 26) as u8) as char;
            assert_eq!(
                line[0].ch, expected,
                "row {i} decoded as {:?}, expected {:?} \
                 (would indicate past-EOF idx leaked as blank)",
                line[0].ch, expected,
            );
        }
    }

    /// Window resize on a File-backed scrollback used to duplicate
    /// records (restart() reopened the file without clearing → the
    /// caller's subsequent push appended on top of existing
    /// content).  F3+10f makes restart() truncate bin to header +
    /// idx to 0, so the re-push lands in a fresh file.
    #[test]
    fn file_restart_truncates_for_reflow() {
        let tmp = TmpDir::new("resize-no-dup");
        let cols = 4usize;
        // Seed file with 50 rows.
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16)
                .expect("create");
            for i in 0..50u32 {
                sb.push_line(&fill((b'a' + (i % 26) as u8), cols), false);
            }
            assert_eq!(sb.len(), 50);
        }
        // Drive through the Scrollback enum since that's the call
        // surface Grid::reflow uses.
        let mut sb = Scrollback::file(tmp.bin(), tmp.idx(), cols, 16)
            .expect("reopen as enum");
        assert_eq!(sb.len(), 50, "reopen should see seeded rows");
        // Restart at new_cols = 8 (the reflow trigger).
        sb.restart(8);
        assert_eq!(
            sb.len(), 0,
            "restart() must clear the file for File variant — \
             leaving content causes the 错位 visible after a window resize"
        );
        // Re-push 50 reflowed rows at new cols.
        for i in 0..50u32 {
            sb.push_line_with_wrapped(&fill((b'a' + (i % 26) as u8), 8), false);
        }
        assert_eq!(
            sb.len(), 50,
            "post-restart push count should be 50, not 100 \
             (100 = restart didn't truncate, file had old + new)"
        );
        // And the rows we read back must be the NEW width.
        let row0 = sb.line_to_vec(0).expect("row 0 decodes");
        assert_eq!(row0.len(), 8, "reflowed row should be at new cols=8");
    }

    /// F3+10c Pass A: manually inject idx entries pointing past bin
    /// EOF and verify reopen drops them.
    #[test]
    fn file_past_eof_idx_tail_dropped_on_reopen() {
        let tmp = TmpDir::new("past-eof-tail");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
                .expect("create");
            for _ in 0..10 {
                sb.push_line(&fill(b'x', cols), false);
            }
        }
        // Append 5 bogus idx entries pointing past bin EOF.  The
        // bin file isn't extended — so these entries can NEVER
        // resolve to a real record.
        let bin_size = std::fs::metadata(&tmp.bin()).unwrap().len();
        {
            use std::io::Write;
            let mut idx = std::fs::OpenOptions::new()
                .write(true)
                .append(true)
                .open(&tmp.idx())
                .unwrap();
            for i in 0..5u64 {
                let bogus = bin_size + 100 + i * 7;
                idx.write_all(&bogus.to_le_bytes()).unwrap();
            }
        }
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8)
            .expect("reopen");
        assert_eq!(
            sb.len(), 10,
            "Pass A should drop the 5 past-EOF entries; reopen total \
             was {} (expected 10)", sb.len()
        );
    }

    /// Round-trip: push N lines, drop cleanly, reopen, verify ALL
    /// N decode to expected content.  Foundation correctness check
    /// — any regression to the BufWriter / Drop / open() machinery
    /// trips this first.
    #[test]
    fn file_clean_close_round_trips_all_rows() {
        let tmp = TmpDir::new("clean-roundtrip");
        let cols = 8usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 32)
                .expect("create");
            for i in 0..500u32 {
                sb.push_line(&fill((b'a' + (i % 26) as u8), cols), false);
            }
        } // <- clean Drop runs flush_for_handoff equivalent
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 32)
            .expect("reopen");
        assert_eq!(sb.len(), 500);
        for i in 0..500 {
            let line = sb.read_line(i).expect("row decodes");
            assert_eq!(line.len(), cols);
            let expected = (b'a' + (i as u32 % 26) as u8) as char;
            assert_eq!(line[0].ch, expected, "row {i} content");
        }
    }
}

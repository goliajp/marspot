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
// The two variants differ a lot in size and that is on purpose: this
// enum exists to avoid trait-object dispatch on the parse hot path
// (~1 % on emoji-dense benches), and boxing the big variant would put
// a pointer chase back in exactly the place the enum was chosen to
// keep clear.
#[allow(clippy::large_enum_variant)]
pub enum Scrollback {
    Memory(MemoryScrollback),
    /// Persistent file-backed scrollback (A1 of the pane upgrade).
    /// Survives L3 self-execv via
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
    ///
    /// RFC-004 A.4 — corrupt-quarantine: when `open()` rejects the
    /// existing pair (bad magic / incompatible version / cell-ABI
    /// mismatch / truncated header / ragged idx), the pair is renamed
    /// to `<name>.corrupt-<unix-secs>` and a fresh empty pair is
    /// opened in its place.  The session keeps its File persistence
    /// (new history keeps landing on disk) and the rejected bytes
    /// stay on disk for forensics instead of being silently shadowed
    /// forever.  Errors that are NOT data-shaped (permissions, ENOSPC,
    /// missing parent dir) propagate — quarantining can't fix those.
    pub fn file(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
    ) -> std::io::Result<Self> {
        match FileScrollback::open(bin_path.clone(), idx_path.clone(), cols, ram_capacity) {
            Ok(f) => Ok(Self::File(f)),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let quarantine = |p: &std::path::Path| {
                    let mut q = p.as_os_str().to_owned();
                    q.push(format!(".corrupt-{ts}"));
                    let _ = std::fs::rename(p, std::path::PathBuf::from(q));
                };
                quarantine(&bin_path);
                quarantine(&idx_path);
                crate::lx_warn!(
                    "scrollback.quarantined",
                    &format!("{e} — pair renamed .corrupt-{ts}, starting fresh"),
                    bin = bin_path.display()
                );
                Ok(Self::File(FileScrollback::open(
                    bin_path,
                    idx_path,
                    cols,
                    ram_capacity,
                )?))
            }
            Err(e) => Err(e),
        }
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
    ///
    /// skips the re-push step.  The historical records stay at their
    /// original widths on disk; display renders them as-is (cells
    /// past `cols` show as default — visually shorter row in a wider
    /// pane, truncated in a narrower one).
    /// Which run of line numbering the current lines belong to.
    ///
    /// Anything filed against a line records this alongside, and
    /// refuses itself when the two disagree -- see `new_epoch`.
    pub fn epoch(&self) -> u64 {
        match self {
            Self::Memory(m) => m.epoch,
            Self::File(f) => f.epoch,
        }
    }

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

    /// Does this variant store the wrapped flag beside the line?
    ///
    /// Only the File one does, and only it survives the process: a
    /// caller that keeps its own mirror has to know whose copy to
    /// believe after an L3 re-exec, when the file outlives the mirror
    /// and the mirror comes back empty.
    /// True when the scrollback itself keeps command marks, so a
    /// reader should believe it over any in-process mirror.
    ///
    /// The same split `keeps_wrapped_flags` draws, for the same reason:
    /// a mirror holds what THIS process pushed, the file holds what the
    /// session ever wrote, and an L3 that re-execs starts with an empty
    /// mirror against a history thousands of lines long.
    pub fn keeps_prompt_marks(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// The command mark on scrollback line `idx`, 0 = oldest.
    pub fn prompt_at(&self, idx: usize) -> crate::grid::PromptMark {
        match self {
            Self::Memory(_) => crate::grid::PromptMark::None,
            Self::File(f) => f.mark_at(idx),
        }
    }

    /// Record a mark against the line most recently pushed.  A no-op
    /// for the in-RAM variant, whose marks live in the grid's mirror.
    pub fn mark_last_line(&mut self, mark: crate::grid::PromptMark) {
        if let Self::File(f) = self {
            f.mark_last_line(mark);
        }
    }

    pub fn keeps_wrapped_flags(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// File variant only: per-line wrapped flag.  Memory returns
    /// false (Grid's `sb_wrapped` is the truth there) — ask
    /// [`Self::keeps_wrapped_flags`] before believing a `false`.
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
        let newest = len - 1 - line_start; // newest line in the window
        let oldest = newest + 1 - avail; // oldest line in the window
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
            // The File half owns its own files; the enum only says
            // which half is in play.
            Self::File(f) => match f.reopen_at_cols(new_cols) {
                Ok(new_f) => Self::File(new_f),
                Err(ram_cap) => Self::Memory(MemoryScrollback::new(ram_cap, new_cols)),
            },
        };
    }
}

mod memory;
pub use memory::MemoryScrollback;

mod file;
pub use file::FileScrollback;

mod format;
mod sidecar;

mod snapshot;
pub use snapshot::FileSnapshot;

#[cfg(test)]
mod testing {
    //! Shared fixtures.  Every test module under `scrollback` is a
    //! descendant of this one, so the temp-dir guard and the row
    //! builders exist once rather than per file.
    use super::format::{FILE_HEADER_BYTES, FILE_MAGIC, read_exact_at};
    use super::*;
    use crate::grid::Cell;

    pub(super) fn fill(b: u8, cols: usize) -> Vec<Cell> {
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
    pub(super) fn alphabet_sb_memory(n: usize, cols: usize) -> Scrollback {
        let mut sb = Scrollback::memory(n, cols);
        for i in 0..n {
            let ch = (b'a' + (i as u8)) as char;
            let line: Vec<Cell> = (0..cols)
                .map(|_| Cell {
                    ch,
                    ..Default::default()
                })
                .collect();
            sb.push_line(&line);
        }
        sb
    }

    // ─── A1: FileScrollback unit tests ──────────────────────────

    /// Temp dir scoped to this test — unique per-call, cleaned on
    /// drop.  No external `tempfile` crate dependency.
    pub(super) struct TmpDir {
        pub(super) path: std::path::PathBuf,
    }
    impl TmpDir {
        pub(super) fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("marspot-scrollback-{label}-{pid}-{n}"));
            std::fs::create_dir_all(&dir).expect("tmpdir create");
            Self { path: dir }
        }
        pub(super) fn bin(&self) -> std::path::PathBuf {
            self.path.join("scrollback.bin")
        }
        pub(super) fn idx(&self) -> std::path::PathBuf {
            self.path.join("scrollback.idx")
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // ─── line identity: which run a local line index belongs to ──────

    pub(super) fn push_n(sb: &mut FileScrollback, n: usize, cols: usize, ch: char) {
        for _ in 0..n {
            let line: Vec<Cell> = (0..cols)
                .map(|_| Cell {
                    ch,
                    ..Default::default()
                })
                .collect();
            sb.push_line(&line, false);
        }
    }

    pub(super) fn header_epoch(bin: &std::path::Path) -> u64 {
        let f = std::fs::File::open(bin).expect("open bin");
        let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
        read_exact_at(&f, &mut hdr, 0).expect("read header");
        u64::from_le_bytes(hdr[24..32].try_into().unwrap())
    }

    /// A row whose cells exercise every attribute and every colour kind,
    /// so a width or byte-order slip shows up as a wrong cell rather
    /// than slipping past on spaces.
    pub(super) fn rich_row(seed: u32, cols: usize) -> Vec<Cell> {
        use crate::grid::{CellAttrs, Color};
        (0..cols)
            .map(|i| {
                let k = seed.wrapping_mul(31).wrapping_add(i as u32);
                let ch = ['a', 'Z', '中', '😀', 'é', '─'][(k % 6) as usize];
                let color = |n: u32| match n % 3 {
                    0 => Color::DEFAULT,
                    1 => Color::indexed((n % 256) as u8),
                    _ => Color::rgb((n % 251) as u8, (n % 241) as u8, (n % 239) as u8),
                };
                Cell {
                    ch,
                    attrs: CellAttrs {
                        fg: color(k),
                        bg: color(k / 3),
                        bold: k.is_multiple_of(2),
                        italic: k.is_multiple_of(3),
                        underline: k.is_multiple_of(5),
                        reverse: k.is_multiple_of(7),
                        dim: k.is_multiple_of(11),
                        ..Default::default()
                    },
                }
            })
            .collect()
    }

    /// Write a scrollback pair exactly as a v2 build did: 13-byte cells,
    /// header version 2, cell_abi 13.  Independent of the v3 writer on
    /// purpose — a compatibility test that produced its "old" file with
    /// the new code would be asserting what it had just set.
    pub(super) fn write_v2_pair(
        bin: &std::path::Path,
        idx: &std::path::Path,
        rows: &[(Vec<Cell>, bool)],
    ) {
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(&FILE_MAGIC.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&13u32.to_le_bytes());
        b.extend_from_slice(&[0u8; 20]);
        let mut offs: Vec<u64> = Vec::new();
        for (row, wrapped) in rows {
            offs.push(b.len() as u64);
            let trimmed = row
                .iter()
                .rposition(|c| *c != Cell::default())
                .map(|i| i + 1)
                .unwrap_or(0);
            let rec_len = (3 + trimmed * 13) as u32;
            b.extend_from_slice(&rec_len.to_le_bytes());
            b.push(*wrapped as u8);
            b.extend_from_slice(&(trimmed as u16).to_le_bytes());
            for c in &row[..trimmed] {
                b.extend_from_slice(&(c.ch as u32).to_le_bytes());
                b.extend_from_slice(&crate::terminal::serialize_attrs_pub(c.attrs));
            }
        }
        offs.push(b.len() as u64);
        std::fs::write(bin, &b).unwrap();
        let ib: Vec<u8> = offs.iter().flat_map(|o| o.to_le_bytes()).collect();
        std::fs::write(idx, ib).unwrap();
    }

    pub(super) fn header_version_and_abi(bin: &std::path::Path) -> (u32, u32) {
        let b = std::fs::read(bin).unwrap();
        (
            u32::from_le_bytes(b[4..8].try_into().unwrap()),
            u32::from_le_bytes(b[8..12].try_into().unwrap()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::testing::*;

    /// RFC-004 A.4 — a corrupt scrollback pair is quarantined
    /// (renamed `.corrupt-<ts>`) and a fresh File pair opens in its
    /// place: persistence continues, the bad bytes stay on disk.
    #[test]
    fn corrupt_pair_is_quarantined_and_reopened_fresh() {
        let dir =
            std::env::temp_dir().join(format!("marspot-sb-quarantine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("scrollback.bin");
        let idx = dir.join("scrollback.idx");
        // Garbage that fails the magic check but passes the ≥32-byte
        // header read.
        std::fs::write(&bin, vec![0xFFu8; 64]).unwrap();
        std::fs::write(&idx, vec![0u8; 8]).unwrap();

        let sb = Scrollback::file(bin.clone(), idx.clone(), 8, 16)
            .expect("quarantine + fresh open must succeed");
        assert!(matches!(sb, Scrollback::File(_)), "must stay File variant");

        // Quarantined originals exist; fresh pair is valid (header only).
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries
                .iter()
                .any(|n| n.starts_with("scrollback.bin.corrupt-")),
            "bin not quarantined: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|n| n.starts_with("scrollback.idx.corrupt-")),
            "idx not quarantined: {entries:?}"
        );
        assert_eq!(
            std::fs::metadata(&bin).unwrap().len(),
            32,
            "fresh bin must be header-only"
        );
        drop(sb);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Reflow is the one path where local line index 0 comes back
    /// meaning a different line, so it is the one that has to move.
    #[test]
    fn reflow_starts_a_new_run() {
        let tmp = TmpDir::new("epoch-reflow");
        let mut sb = Scrollback::file(tmp.bin(), tmp.idx(), 8, 4).expect("open");
        let line: Vec<Cell> = (0..8)
            .map(|_| Cell {
                ch: 'a',
                ..Default::default()
            })
            .collect();
        sb.push_line(&line);
        let before = sb.epoch();
        assert!(sb.is_persistent(), "the fixture has to be the file variant");

        sb.restart(12);
        assert_ne!(sb.epoch(), before, "index 0 now means a different line");
        assert_eq!(
            header_epoch(&tmp.bin()),
            sb.epoch(),
            "and the file says so too"
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
}

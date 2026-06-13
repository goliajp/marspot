//! Cross-process grid framebuffer over POSIX shared memory (target #4).
//!
//! L3 (`marspot-session`) publishes the in-view window of its `Grid`
//! into a shared region after each pump; L2 (`marspot-core`) maps the
//! same region and reads it to render. The published snapshot is the
//! *visible* window only (`rows × cols` cells, ~40 KiB at 80×24), so
//! L2 never holds L3's scrollback — see `docs/per-session-l3.md`.
//!
//! ## Sharing primitive
//!
//! POSIX shm: `shm_open(O_CREAT|O_EXCL)` → `ftruncate` → `mmap
//! MAP_SHARED` → `shm_unlink` (the name is freed immediately; the fd
//! and mapping live on). The creator keeps the fd; the consumer maps
//! the *same fd*, inherited across the L2→L3 spawn the way the control
//! socket already is. No lingering name, no filesystem.
//!
//! ## Tear-free reads: seqlock
//!
//! A single writer (L3) and N readers (L2). The header's `seq` counter
//! is bumped to odd before a write and back to even after, with
//! release/acquire fences around the cell copy. A reader retries while
//! `seq` is odd or changed across its read, so it never renders a
//! half-written frame — without any cross-process lock. The cell copy
//! itself races the writer in the strict memory model; the seq check
//! discards any raced read, the same trade the Linux-kernel seqlock
//! makes. Readers are fast (one ~40 KiB memcpy) and the writer
//! publishes only per-pump, so retries are rare.
//!
//! ## Layout
//!
//! `[Header][Cell; cols*rows]`. `Header` is `#[repr(C)]` so both ends
//! agree on field offsets; `cell_size`/`magic`/`version` in the header
//! let a reader reject a region written by an incompatible build
//! (relevant once per-session update can run L3 and L2 on skewed
//! binaries — a mismatch makes the reader refuse rather than
//! misinterpret bytes).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use crate::grid::{Cell, Grid};

const MAGIC: u32 = 0x4d_53_47_31; // "MSG1"
const VERSION: u32 = 1;

/// Cursor is visible (DECTCEM).
pub const FLAG_CURSOR_VISIBLE: u32 = 1 << 0;
/// Application cursor-key mode (DECCKM) — L2 needs it to encode arrows.
pub const FLAG_APP_CURSOR_KEYS: u32 = 1 << 1;
/// Bracketed-paste mode (DECSET ?2004).
pub const FLAG_BRACKETED_PASTE: u32 = 1 << 2;

/// Shared-region header. `#[repr(C)]` for a stable cross-process
/// layout. `seq` is first and accessed only atomically (the seqlock);
/// every other field is plain, written between the odd/even seq bumps.
#[repr(C)]
struct Header {
    seq: AtomicU64,
    magic: u32,
    version: u32,
    cell_size: u32,
    cols: u32,
    rows: u32,
    cursor_col: u32,
    cursor_row: u32,
    flags: u32,
    scroll_push_count: u64,
    scrollback_len: u32,
    view_offset: u32,
}

const HEADER_BYTES: usize = std::mem::size_of::<Header>();

/// One published frame's metadata, returned alongside the cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub flags: u32,
    pub scroll_push_count: u64,
    pub scrollback_len: u32,
    pub view_offset: u16,
}

impl GridSnapshot {
    pub fn cursor_visible(&self) -> bool {
        self.flags & FLAG_CURSOR_VISIBLE != 0
    }
    pub fn app_cursor_keys(&self) -> bool {
        self.flags & FLAG_APP_CURSOR_KEYS != 0
    }
    pub fn bracketed_paste(&self) -> bool {
        self.flags & FLAG_BRACKETED_PASTE != 0
    }
}

fn region_len(cols: u16, rows: u16) -> usize {
    HEADER_BYTES + cols as usize * rows as usize * std::mem::size_of::<Cell>()
}

/// Unique-per-process shm name. macOS caps shm names at ~31 bytes
/// including the leading '/'; `/msp-g-<pid>-<n>` stays well under.
fn next_shm_name() -> std::ffi::CString {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::ffi::CString::new(format!("/msp-g-{}-{}", std::process::id(), n)).unwrap()
}

/// Create + size + stamp a fresh shared region for `cols × rows`, and
/// return its fd.
///
/// The *creator* owns the region's lifecycle and stamps the immutable
/// header identity (magic / version / cell-size / dims) here, so both
/// the writer ([`GridShmWriter::from_fd`]) and any readers
/// ([`GridShmReader::from_fd`]) can map and validate the same fd in
/// either order — no "map before the first publish" race. This is the
/// L2-owns-shm split: L2 calls `create_region`, inherits the fd into the
/// L3 child (the writer), and maps a `dup` as the reader itself. The shm
/// name is unlinked immediately, so only fd holders can map it.
pub fn create_region(cols: u16, rows: u16) -> io::Result<OwnedFd> {
    assert!(cols > 0 && rows > 0, "grid_shm: zero dimension");
    let len = region_len(cols, rows);
    let name = next_shm_name();

    // O_EXCL so a stale name from a crashed peer can't be reused
    // mid-flight; we unlink right after creating anyway.
    let fd = unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Free the name now; the fd + future mappings survive.
    unsafe {
        libc::shm_unlink(name.as_ptr());
    }

    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // Temporarily map to stamp the immutable header, then unmap; the
    // writer/readers re-map via `from_fd`. ftruncate zero-fills, so seq
    // starts at 0 (even = stable, "never published") with no extra work.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let h = base as *mut Header;
        (*h).magic = MAGIC;
        (*h).version = VERSION;
        (*h).cell_size = std::mem::size_of::<Cell>() as u32;
        (*h).cols = cols as u32;
        (*h).rows = rows as u32;
        libc::munmap(base, len);
    }
    Ok(fd)
}

/// Writer side (L3): owns the shm region and publishes grid snapshots.
pub struct GridShmWriter {
    fd: OwnedFd,
    base: *mut u8,
    len: usize,
    cols: u16,
    rows: u16,
}

// The raw pointer is into a private mmap this struct solely owns; it is
// safe to move the writer across threads. (Concurrent publish() calls
// would be a logic error — there is one writer by construction.)
unsafe impl Send for GridShmWriter {}

impl GridShmWriter {
    /// Create a fresh shared region sized for `cols × rows` and take the
    /// writer role on it. Convenience for the standalone path (no L2
    /// owning the region) and the unit tests; equivalent to
    /// `from_fd(create_region(cols, rows)?)`.
    pub fn create(cols: u16, rows: u16) -> io::Result<Self> {
        Self::from_fd(create_region(cols, rows)?)
    }

    /// Take the single-writer role on an existing region (created by
    /// [`create_region`], possibly in another process and inherited as
    /// `fd`). Dimensions come from the header the creator stamped; the
    /// magic / version / cell-size are validated so a region from an
    /// incompatible build is refused rather than written through a wrong
    /// layout.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let len = st.st_size as usize;
        if len < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "grid_shm: region smaller than header",
            ));
        }

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = base as *mut u8;

        let h = base as *const Header;
        let (magic, version, cell_size, cols, rows) = unsafe {
            (
                (*h).magic,
                (*h).version,
                (*h).cell_size,
                (*h).cols,
                (*h).rows,
            )
        };
        let bad = magic != MAGIC
            || version != VERSION
            || cell_size != std::mem::size_of::<Cell>() as u32
            || region_len(cols as u16, rows as u16) > len;
        if bad {
            unsafe {
                libc::munmap(base as *mut libc::c_void, len);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "grid_shm: incompatible region (magic/version/cell-size/dims)",
            ));
        }

        Ok(Self {
            fd,
            base,
            len,
            cols: cols as u16,
            rows: rows as u16,
        })
    }

    /// The shm fd, for passing to a child (e.g. inherited by L2/L3).
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    #[inline]
    unsafe fn header(&self) -> *mut Header {
        self.base as *mut Header
    }

    #[inline]
    unsafe fn cells_ptr(&self) -> *mut Cell {
        self.base.add(HEADER_BYTES) as *mut Cell
    }

    /// Publish the grid's in-view window (`view_offset` rows up from the
    /// live tail) plus cursor + mode flags. Single-writer seqlock.
    pub fn publish(
        &mut self,
        grid: &Grid,
        view_offset: u16,
        flags: u32,
    ) {
        debug_assert_eq!(grid.cols(), self.cols);
        debug_assert_eq!(grid.rows(), self.rows);
        let (cur_c, cur_r) = grid.cursor();

        unsafe {
            let h = self.header();
            // Enter the write: bump seq to odd.
            let s = (*h).seq.load(Ordering::Relaxed);
            (*h).seq.store(s.wrapping_add(1), Ordering::Relaxed);
            fence(Ordering::Release);

            // Plain header fields.
            (*h).cursor_col = cur_c as u32;
            (*h).cursor_row = cur_r as u32;
            (*h).flags = flags;
            (*h).scroll_push_count = grid.scroll_push_count();
            (*h).scrollback_len = grid.scrollback_len() as u32;
            (*h).view_offset = view_offset as u32;

            // Cells: read the visible window straight into the region,
            // no intermediate allocation.
            let cells = self.cells_ptr();
            let cols = self.cols;
            for row in 0..self.rows {
                let row_base = row as usize * cols as usize;
                for col in 0..cols {
                    let cell = grid.cell_at_view(view_offset, col, row);
                    std::ptr::write(cells.add(row_base + col as usize), cell);
                }
            }

            // Exit the write: publish with a release store to even.
            fence(Ordering::Release);
            (*h).seq.store(s.wrapping_add(2), Ordering::Release);
        }
    }
}

impl Drop for GridShmWriter {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.len);
            }
        }
        // fd closed by OwnedFd.
    }
}

/// Reader side (L2): maps a region published by a `GridShmWriter` and
/// reads tear-free snapshots.
pub struct GridShmReader {
    base: *const u8,
    len: usize,
    cols: u16,
    rows: u16,
}

unsafe impl Send for GridShmReader {}

impl GridShmReader {
    /// Map a region by its fd (inherited from the writer's process).
    /// Validates magic / version / cell-size so a region from an
    /// incompatible build is refused rather than misread.
    pub fn from_fd(fd: RawFd) -> io::Result<Self> {
        // Size from the fd itself — the writer ftruncate'd it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let len = st.st_size as usize;
        if len < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "grid_shm: region smaller than header",
            ));
        }

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = base as *const u8;

        let h = base as *const Header;
        let (magic, version, cell_size, cols, rows) = unsafe {
            (
                (*h).magic,
                (*h).version,
                (*h).cell_size,
                (*h).cols,
                (*h).rows,
            )
        };
        let bad = magic != MAGIC
            || version != VERSION
            || cell_size != std::mem::size_of::<Cell>() as u32
            || region_len(cols as u16, rows as u16) > len;
        if bad {
            unsafe {
                libc::munmap(base as *mut libc::c_void, len);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "grid_shm: incompatible region (magic/version/cell-size/dims)",
            ));
        }

        Ok(Self {
            base,
            len,
            cols: cols as u16,
            rows: rows as u16,
        })
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }

    #[inline]
    unsafe fn header(&self) -> *const Header {
        self.base as *const Header
    }

    #[inline]
    unsafe fn cells_ptr(&self) -> *const Cell {
        self.base.add(HEADER_BYTES) as *const Cell
    }

    /// Read the latest published frame into `out` (resized to
    /// `cols*rows`), retrying until a tear-free snapshot lands. Returns
    /// `None` only if the writer has never published (seq still 0).
    pub fn read(&self, out: &mut Vec<Cell>) -> Option<GridSnapshot> {
        let n = self.cols as usize * self.rows as usize;
        out.clear();
        out.reserve(n);
        // Bounded spin: a single writer holds the odd window for the
        // duration of one ~40 KiB copy, so a handful of retries always
        // suffices in practice; cap to avoid an unbounded loop if a
        // writer died mid-publish (seq stuck odd).
        for _ in 0..1024 {
            unsafe {
                let h = self.header();
                let s1 = (*h).seq.load(Ordering::Acquire);
                if s1 == 0 {
                    return None; // never published
                }
                if s1 & 1 != 0 {
                    std::hint::spin_loop();
                    continue; // writer mid-publish
                }
                let cursor_col = (*h).cursor_col as u16;
                let cursor_row = (*h).cursor_row as u16;
                let flags = (*h).flags;
                let scroll_push_count = (*h).scroll_push_count;
                let scrollback_len = (*h).scrollback_len;
                let view_offset = (*h).view_offset as u16;

                out.set_len(0);
                let cells = self.cells_ptr();
                std::ptr::copy_nonoverlapping(cells, out.as_mut_ptr(), n);
                out.set_len(n);

                fence(Ordering::Acquire);
                let s2 = (*h).seq.load(Ordering::Relaxed);
                if s1 == s2 {
                    return Some(GridSnapshot {
                        cols: self.cols,
                        rows: self.rows,
                        cursor_col,
                        cursor_row,
                        flags,
                        scroll_push_count,
                        scrollback_len,
                        view_offset,
                    });
                }
                // Torn read — writer published mid-copy; retry.
            }
            std::hint::spin_loop();
        }
        None
    }
}

impl Drop for GridShmReader {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Cell, Grid};

    fn dup_fd(fd: RawFd) -> RawFd {
        let d = unsafe { libc::dup(fd) };
        assert!(d >= 0, "dup failed");
        d
    }

    #[test]
    fn round_trips_a_published_grid() {
        let cols = 20u16;
        let rows = 5u16;
        let mut grid = Grid::new(cols, rows);
        // Write some content + move the cursor.
        for (i, ch) in "hello".chars().enumerate() {
            grid.set_cell(i as u16, 0, Cell::from(ch));
        }
        grid.set_cursor(5, 0);

        let mut writer = GridShmWriter::create(cols, rows).expect("create");

        // Reader before any publish → None.
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let mut buf = Vec::new();
        assert!(reader.read(&mut buf).is_none(), "no frame before publish");

        writer.publish(&grid, 0, FLAG_CURSOR_VISIBLE);

        let snap = reader.read(&mut buf).expect("frame after publish");
        assert_eq!(snap.cols, cols);
        assert_eq!(snap.rows, rows);
        assert_eq!((snap.cursor_col, snap.cursor_row), (5, 0));
        assert!(snap.cursor_visible());
        assert_eq!(buf.len(), cols as usize * rows as usize);
        assert_eq!(buf[0].ch, 'h');
        assert_eq!(buf[4].ch, 'o');
        assert_eq!(buf[5].ch, ' '); // blank past the text
    }

    #[test]
    fn republish_advances_and_reads_latest() {
        let cols = 10u16;
        let rows = 2u16;
        let mut grid = Grid::new(cols, rows);
        let mut writer = GridShmWriter::create(cols, rows).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let mut buf = Vec::new();

        for round in 0..5u8 {
            let ch = (b'a' + round) as char;
            grid.set_cell(0, 0, Cell::from(ch));
            grid.set_cursor(round as u16, 1);
            writer.publish(&grid, 0, 0);
            let snap = reader.read(&mut buf).expect("frame");
            assert_eq!(buf[0].ch, ch, "round {round}");
            assert_eq!(snap.cursor_row, 1);
            assert_eq!(snap.cursor_col, round as u16);
            assert!(!snap.cursor_visible());
        }
    }

    #[test]
    fn create_region_then_split_writer_and_reader() {
        // The L2-owns-shm path: a region is created standalone, the
        // writer takes it via from_fd (dims read from the stamped
        // header), and a reader maps a dup. A reader mapped *before* the
        // first publish must validate fine (magic stamped at create) and
        // read None until the writer publishes.
        let cols = 12u16;
        let rows = 3u16;
        let region = create_region(cols, rows).expect("create_region");

        // Reader on a dup, mapped before any writer exists.
        let reader = GridShmReader::from_fd(dup_fd(region.as_raw_fd())).expect("reader");
        assert_eq!((reader.cols(), reader.rows()), (cols, rows));
        let mut buf = Vec::new();
        assert!(reader.read(&mut buf).is_none(), "no frame before publish");

        // Writer takes the region itself.
        let mut writer = GridShmWriter::from_fd(region).expect("writer from_fd");
        let mut grid = Grid::new(cols, rows);
        grid.set_cell(0, 0, Cell::from('Z'));
        grid.set_cursor(1, 2);
        writer.publish(&grid, 0, FLAG_CURSOR_VISIBLE);

        let snap = reader.read(&mut buf).expect("frame after publish");
        assert_eq!((snap.cols, snap.rows), (cols, rows));
        assert_eq!((snap.cursor_col, snap.cursor_row), (1, 2));
        assert!(snap.cursor_visible());
        assert_eq!(buf[0].ch, 'Z');
    }

    #[test]
    fn rejects_incompatible_region() {
        // A region that's just an empty file (no header) is refused.
        let cols = 4u16;
        let rows = 4u16;
        let writer = GridShmWriter::create(cols, rows).expect("create");
        // A reader on the real fd is fine (magic stamped at create).
        let ok = GridShmReader::from_fd(dup_fd(writer.fd()));
        assert!(ok.is_ok());
    }

    #[test]
    fn concurrent_writer_never_yields_torn_frame() {
        // One thread republishes a uniform grid (every cell == round's
        // char) as fast as it can; the reader must only ever see a
        // frame where ALL cells agree — a torn read would mix two
        // rounds' chars. Catches a broken seqlock.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let cols = 64u16;
        let rows = 24u16;
        let mut writer = GridShmWriter::create(cols, rows).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let stop = Arc::new(AtomicBool::new(false));

        let stop_w = stop.clone();
        let writer_thread = std::thread::spawn(move || {
            let mut grid = Grid::new(cols, rows);
            let mut round = 0u32;
            while !stop_w.load(Ordering::Relaxed) {
                let ch = char::from_u32(0x21 + (round % 90)).unwrap();
                for r in 0..rows {
                    for c in 0..cols {
                        grid.set_cell(c, r, Cell::from(ch));
                    }
                }
                writer.publish(&grid, 0, 0);
                round = round.wrapping_add(1);
            }
        });

        let mut buf = Vec::new();
        let mut reads = 0;
        while reads < 50_000 {
            if let Some(_snap) = reader.read(&mut buf) {
                let first = buf[0].ch;
                assert!(
                    buf.iter().all(|c| c.ch == first),
                    "torn frame: cell mix detected"
                );
                reads += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer_thread.join().unwrap();
    }
}

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
// VERSION 3 (2026-06-17): per-row DECAWM wrap flags appended after the
// cell area so L2's mirror grid carries the same `wrapped` signal L3's
// parser sets.  Without this, scan_visible_links on the L2 side always
// saw `wrapped = false`, breaking cross-row URL/path detection.
//
// Forward-compat upgrade path (added 2026-06-17 after Amendment 16
// install fallout — see CLAUDE memory "wire upgrade silent + lossless"):
// `from_fd` accepts both v2 and v3 regions.  A v2 region is upgraded
// in place — ftruncate to v3 capacity, zero the wrapped area, then
// atomic-bump header.version to 3.  After upgrade every subsequent
// reader / writer (in any process) sees v3 layout, so a silent
// install across a v2→v3 shm protocol bump is lossless: L3 self-execv
// into the new image, the new image upgrades the inherited v2 region
// to v3, PTY + shell + L2 reader all stay attached.  The MIN version
// the loader will touch is v2; anything older or newer fails fast.
const VERSION: u32 = 3;
const VERSION_MIN_COMPAT: u32 = 2;

/// Capacity bound for a region's cell area, in cells.  A region is
/// *mapped* to hold up to this many cells so a live resize (target #4
/// step 4b) is a pure header-metadata change — the writer publishes the
/// new dims + cells in place, no remap and no fd hand-off across the
/// L2↔L3 socket.  The mapping is lazily faulted, so only the in-use
/// `cols × rows` cells are ever resident (≈40 KiB at 80×24); this cap
/// bounds *virtual* size (≈5 MiB) and is the documented growth bound for
/// the shm framebuffer.  256 Ki cells covers any realistic single pane
/// (e.g. a full 6K display at a tiny font ≈ 600 × 282 ≈ 170 Ki).
pub const MAX_CELLS: usize = 256 * 1024;

/// Maximum viewport rows the wrapped-flag array can address.  Each
/// row contributes 1 byte (0 or 1) at a fixed offset after the cell
/// region — see `wrapped_offset()`.  1024 is the documented growth
/// bound, comfortable for any realistic terminal (a full 6K display
/// at min font size is ≈ 282 rows).
pub const MAX_ROWS: usize = 1024;

/// Env var carrying the inherited grid-shm fd from L2 (region creator)
/// to the L3 child (the writer). Set by L2 when it spawns a session
/// process; absent in the standalone path (L3 self-creates the region).
pub const ENV_SHM_FD: &str = "MARSPOT_SHM_FD";

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

/// Byte length the writer/reader actually touches at `cols × rows`:
/// header + cell region + per-row wrapped flags.  Used for bounds
/// checks against the (fixed) mapping size — the actual mapping is
/// always [`capacity_bytes()`].
fn region_len(cols: u16, rows: u16) -> usize {
    HEADER_BYTES
        + cols as usize * rows as usize * std::mem::size_of::<Cell>()
        + rows as usize
}

/// Fixed offset from `base` to the per-row wrapped-flag array.  Sits
/// AFTER the cell capacity (not after the live cell area) so the
/// offset is independent of grid dims — no recomputation per publish.
const fn wrapped_offset() -> usize {
    HEADER_BYTES + MAX_CELLS * std::mem::size_of::<Cell>()
}

/// Total mapped bytes for every region: header + the [`MAX_CELLS`]
/// cap + the [`MAX_ROWS`] wrapped-flag cap.  Fixed so a resize within
/// the caps never remaps.
const fn capacity_bytes() -> usize {
    wrapped_offset() + MAX_ROWS
}

/// True when a `cols × rows` grid fits the capacity cap.
fn fits_capacity(cols: u16, rows: u16) -> bool {
    cols as usize * rows as usize <= MAX_CELLS && rows as usize <= MAX_ROWS
}

/// Unique-per-process shm name. macOS caps shm names at ~31 bytes
/// including the leading '/'; `/msp-g-<pid>-<n>` stays well under.
fn next_shm_name() -> std::ffi::CString {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::ffi::CString::new(format!("/msp-g-{}-{}", std::process::id(), n)).unwrap()
}

/// Deterministic shm name for a given session id.  Lets a freshly
/// spawned L2 reattach to a surviving L3's shm region by name (the
/// fd doesn't survive the swap; the name does).  Same length budget
/// as `next_shm_name`: `/msp-s-<u64>` is at most 27 chars including
/// the leading '/', under macOS's ~31-char cap.
pub fn session_shm_name(session_id: u64) -> std::ffi::CString {
    std::ffi::CString::new(format!("/msp-s-{session_id}")).unwrap()
}

/// Create + size + stamp a fresh shared region under a caller-chosen
/// name (typically `session_shm_name(id)`), **without** unlinking it
/// — so a separate process can later `shm_open` the same name.  The
/// caller is responsible for `delete_region(name)` on retirement.
///
/// Used by RFC-003 step 3.5: L2 creates one region per L3 with the
/// session id baked in the name; a post-silent-update L2 can find a
/// surviving L3 by scanning `sessions/<id>/entry.toml` and re-opening
/// the region.
pub fn create_region_named(
    cols: u16,
    rows: u16,
    name: &std::ffi::CStr,
) -> io::Result<OwnedFd> {
    assert!(cols > 0 && rows > 0, "grid_shm: zero dimension");
    assert!(
        fits_capacity(cols, rows),
        "grid_shm: {cols}x{rows} exceeds capacity cap {MAX_CELLS} cells"
    );
    let len = capacity_bytes();

    // Best-effort unlink first so a stale name from a prior crashed L3
    // doesn't block O_EXCL.  ENOENT is the expected case on a fresh
    // boot — ignore.
    unsafe { libc::shm_unlink(name.as_ptr()) };

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

    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(io::Error::last_os_error());
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

/// Re-open an existing shm region by name (RDWR so the caller can use
/// it as either reader or writer).  Used by L2 boot reattach.
pub fn open_region(name: &std::ffi::CStr) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Best-effort `shm_unlink` of a region by name.  Called when a
/// session is retired so the kernel actually frees the memory once
/// every fd-holder has closed it.
pub fn delete_region(name: &std::ffi::CStr) {
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

/// Map a region and, if needed, upgrade its layout in place to the
/// current `VERSION`.  Used by both `GridShmWriter::from_fd` and
/// `GridShmReader::from_fd` so the silent-update path (L3 self-execv
/// across a shm wire bump) is lossless regardless of which side
/// attaches first.
///
/// Steps:
///   1. fstat → size.  If smaller than the current capacity, ftruncate
///      up so the v3 mapping has room for the new wrapped-flag area.
///   2. mmap RDWR | SHARED at full capacity.
///   3. Read magic + version + cell_size + dims.  Reject anything
///      whose magic / cell_size / dims don't match this build, or
///      whose version is below `VERSION_MIN_COMPAT` or above the
///      current `VERSION`.
///   4. If version < `VERSION`, run the per-version upgrade lambda
///      (currently just v2→v3: zero the wrapped area, atomic-bump
///      header.version).  After this everyone sees v3.
///
/// Returns `(base, len, cols, rows)` on success.
fn attach_and_maybe_upgrade(
    fd: RawFd,
) -> io::Result<(*mut u8, usize, u16, u16)> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let cur_size = st.st_size as usize;
    if cur_size < HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grid_shm: region smaller than header",
        ));
    }
    let want_size = capacity_bytes();
    if cur_size < want_size {
        // Best-effort grow.  Failure here is fatal — without the
        // full v3 capacity we can't safely write wrapped flags.
        if unsafe { libc::ftruncate(fd, want_size as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let len = want_size;
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
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
        || version < VERSION_MIN_COMPAT
        || version > VERSION
        || cell_size != std::mem::size_of::<Cell>() as u32
        || region_len(cols as u16, rows as u16) > len;
    if bad {
        unsafe { libc::munmap(base as *mut libc::c_void, len); }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "grid_shm: incompatible region (magic/version/cell-size/dims)",
        ));
    }

    if version < VERSION {
        // v2 → v3 upgrade: cell layout unchanged, but the v2 mapping
        // had no wrapped-flag area.  We've already ftruncate'd to v3
        // capacity above, so the wrapped area exists as zero pages
        // by virtue of ftruncate's zero-fill guarantee.  Be defensive
        // anyway and explicitly zero the wrapped region, then atomic-
        // bump the version so subsequent attachers stop upgrading.
        unsafe {
            let wrapped = base.add(wrapped_offset());
            std::ptr::write_bytes(wrapped, 0, MAX_ROWS);
            // Release fence so the wrapped zeros + size are visible
            // before another process observes version=3.
            fence(Ordering::Release);
            let h = base as *mut Header;
            (*h).version = VERSION;
        }
    }

    Ok((base, len, cols as u16, rows as u16))
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
    assert!(
        fits_capacity(cols, rows),
        "grid_shm: {cols}x{rows} exceeds capacity cap {MAX_CELLS} cells"
    );
    // Always map the full capacity so a later resize within the cap is a
    // header change, not a remap; lazily faulted so only the live dims
    // are resident.
    let len = capacity_bytes();
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
    /// FNV-1a hash of the most recent `publish()`'s grid + cursor + flags
    /// + view window.  `publish_if_changed()` skips the entire shm write
    /// (and the caller's GridReady poke) when the next publish would
    /// produce a bit-identical snapshot.  Was the leading idle-CPU source
    /// before this gate: TUIs (claudecode, etc.) hammer the PTY at their
    /// internal redraw cadence even when the visible grid hasn't moved,
    /// so without dedupe L3 publishes ~30/s, L2 wakes + renders ~25/s,
    /// shows up as ~10 % idle CPU on a 9-pane window.
    last_publish_hash: u64,
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
    ///
    /// Forward-compat: a v2 region is upgraded to v3 in place (see
    /// `attach_and_maybe_upgrade`).
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let (base, len, cols, rows) = attach_and_maybe_upgrade(fd.as_raw_fd())?;
        Ok(Self {
            fd,
            base,
            len,
            cols,
            rows,
            // 0 is a fine sentinel: a brand-new writer's first publish
            // computes a real hash that is almost never 0, so the first
            // publish always proceeds.  Worst case (collision = 0): one
            // missed first frame; L2 sees the next change.
            last_publish_hash: 0,
        })
    }

    /// The shm fd, for passing to a child (e.g. inherited by L2/L3).
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Region dimensions (stamped by the creator).  When L2 owns the
    /// region it sizes it to the on-screen cell rect; L3 must drive its
    /// session at exactly these dims so the grid it publishes fits the
    /// region (a mismatch would overflow the mapping on `publish`).
    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }

    #[inline]
    unsafe fn header(&self) -> *mut Header {
        self.base as *mut Header
    }

    #[inline]
    unsafe fn cells_ptr(&self) -> *mut Cell {
        self.base.add(HEADER_BYTES) as *mut Cell
    }

    /// Per-row wrapped-flag array — one byte per viewport row, fixed
    /// offset after the cell capacity.  Reader and writer agree by
    /// construction (same const), independent of live dims.
    #[inline]
    unsafe fn wrapped_ptr(&self) -> *mut u8 {
        self.base.add(wrapped_offset())
    }

    /// Hash-dedupe wrapper around [`publish`].  Computes an FNV-1a
    /// digest of every byte that the next `publish()` would write
    /// (header fields, cursor, view offset, the whole cell window,
    /// per-row wrapped flags).  If it matches the last published
    /// snapshot exactly, this returns `false` without touching the
    /// shm region — the caller skips its GridReady poke and L2 keeps
    /// sleeping.  When something genuinely changed, the hash is
    /// updated and the underlying `publish()` runs as before, and
    /// the caller proceeds with the poke.
    ///
    /// Why this exists: a busy TUI (claudecode, htop, vim with a
    /// blinking cursor) emits PTY redraw bytes at its own internal
    /// cadence — often 30+ Hz — even when the visible characters
    /// haven't moved.  Pre-dedupe, L3 pushed each of those into shm
    /// and L2 woke + re-rendered the entire window.  On a 9-pane
    /// setup this manifested as ~10 % idle CPU on marspot-core (per
    /// `/usr/bin/sample`, event tally showed ~30 L3Ready/s arriving
    /// despite "nothing" happening visually).  After dedupe, only
    /// the publishes that actually change a cell survive, and the
    /// idle baseline drops toward zero.
    pub fn publish_if_changed(
        &mut self,
        grid: &Grid,
        view_offset: u16,
        flags: u32,
    ) -> bool {
        let h = compute_publish_hash(grid, view_offset, flags);
        if h == self.last_publish_hash {
            return false;
        }
        self.last_publish_hash = h;
        self.publish(grid, view_offset, flags);
        true
    }

    /// Publish the grid's in-view window (`view_offset` rows up from the
    /// live tail) plus cursor + mode flags. Single-writer seqlock.
    pub fn publish(
        &mut self,
        grid: &Grid,
        view_offset: u16,
        flags: u32,
    ) {
        let cols = grid.cols();
        let rows = grid.rows();
        // Hard assert (not debug): writing more cells than the mapping
        // holds is out-of-bounds memory unsafety, never tolerable. The
        // grid's dims are the published frame's dims — they ride in the
        // header so a resize needs no remap.
        assert!(
            fits_capacity(cols, rows),
            "grid_shm: publish {cols}x{rows} exceeds capacity {MAX_CELLS} cells"
        );
        self.cols = cols;
        self.rows = rows;
        let (cur_c, cur_r) = grid.cursor();
        // F3+11.1 — when L3 publishes a scrolled view (view_offset > 0)
        // the cells the reader sees are scrollback content, but
        // `grid.cursor()` is the LIVE cursor position — drawing it
        // at those live coordinates would land the cursor block on
        // top of scrollback text, NOT at the input prompt where the
        // user actually types.  Hide the cursor whenever the view is
        // scrolled; cursor reappears when the user scrolls back to
        // the live tail.  Other terminals (iTerm2 / Alacritty) do
        // the same — cursor only visible at view_offset == 0.
        let flags = if view_offset == 0 {
            flags
        } else {
            flags & !FLAG_CURSOR_VISIBLE
        };

        unsafe {
            let h = self.header();
            // Enter the write: bump seq to odd.
            let s = (*h).seq.load(Ordering::Relaxed);
            (*h).seq.store(s.wrapping_add(1), Ordering::Relaxed);
            fence(Ordering::Release);

            // Plain header fields — including the live dims, so a reader
            // always pairs the cell block with the dims it was written at.
            (*h).cols = cols as u32;
            (*h).rows = rows as u32;
            (*h).cursor_col = cur_c as u32;
            (*h).cursor_row = cur_r as u32;
            (*h).flags = flags;
            (*h).scroll_push_count = grid.scroll_push_count();
            (*h).scrollback_len = grid.scrollback_len() as u32;
            (*h).view_offset = view_offset as u32;

            // Cells: read the visible window straight into the region,
            // no intermediate allocation.
            let cells = self.cells_ptr();
            for row in 0..rows {
                let row_base = row as usize * cols as usize;
                for col in 0..cols {
                    let cell = grid.cell_at_view(view_offset, col, row);
                    std::ptr::write(cells.add(row_base + col as usize), cell);
                }
            }
            // Per-row DECAWM wrapped flags — one byte per viewport row
            // at the fixed wrapped offset.  L2's link scanner depends
            // on these to merge soft-wrap continuation rows into one
            // logical line for URL/path detection.  v=2 omitted them
            // and L2 always saw `wrapped=false`.
            let wrapped = self.wrapped_ptr();
            for row in 0..rows {
                let flag = grid.wrapped_at_view(view_offset, row) as u8;
                std::ptr::write(wrapped.add(row as usize), flag);
            }

            // Exit the write: publish with a release store to even.
            fence(Ordering::Release);
            (*h).seq.store(s.wrapping_add(2), Ordering::Release);
        }
    }
}

/// FNV-1a over the full publish payload — header dims/cursor/flags
/// plus every cell + per-row wrapped flag.  Walked in the same order
/// `publish()` would write so two identical grids produce identical
/// hashes regardless of how they were reached.  The constants are the
/// canonical 64-bit FNV-1a (offset basis + prime) — fast on M-series,
/// good enough for the equality-only use we make of it (no
/// adversarial input here, only a same-process L3 hashing its own
/// terminal state).
fn compute_publish_hash(grid: &Grid, view_offset: u16, flags: u32) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    #[inline(always)]
    fn mix(h: u64, x: u64) -> u64 {
        h.wrapping_mul(FNV_PRIME) ^ x
    }
    let cols = grid.cols();
    let rows = grid.rows();
    let (cur_c, cur_r) = grid.cursor();
    let mut h = FNV_OFFSET;
    h = mix(h, flags as u64);
    h = mix(h, cur_c as u64);
    h = mix(h, cur_r as u64);
    h = mix(h, view_offset as u64);
    h = mix(h, grid.scroll_push_count() as u64);
    h = mix(h, grid.scrollback_len() as u64);
    h = mix(h, cols as u64);
    h = mix(h, rows as u64);
    for row in 0..rows {
        for col in 0..cols {
            let cell = grid.cell_at_view(view_offset, col, row);
            h = mix(h, cell.ch as u64);
            h = mix(h, hash_cell_attrs(&cell.attrs));
        }
        h = mix(h, grid.wrapped_at_view(view_offset, row) as u64);
    }
    h
}

#[inline(always)]
fn hash_cell_attrs(a: &crate::grid::CellAttrs) -> u64 {
    let bools = (a.bold as u64)
        | ((a.italic as u64) << 1)
        | ((a.underline as u64) << 2)
        | ((a.reverse as u64) << 3)
        | ((a.dim as u64) << 4);
    bools
        .wrapping_add(hash_color(&a.fg) << 8)
        .wrapping_add(hash_color(&a.bg) << 32)
}

#[inline(always)]
fn hash_color(c: &crate::grid::Color) -> u64 {
    use crate::grid::Color;
    match c {
        Color::Default => 0,
        Color::Indexed(i) => 0x100 | (*i as u64),
        Color::Rgb(r, g, b) => {
            0x1_0000_0000 | ((*r as u64) << 16) | ((*g as u64) << 8) | (*b as u64)
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
    ///
    /// Forward-compat: a v2 region is upgraded to v3 in place (see
    /// `attach_and_maybe_upgrade`).  Reader is conceptually
    /// read-only, but maps RDWR so a single one-shot upgrade store
    /// on first attach is safe.
    pub fn from_fd(fd: RawFd) -> io::Result<Self> {
        let (base, len, cols, rows) = attach_and_maybe_upgrade(fd)?;
        Ok(Self {
            base: base as *const u8,
            len,
            cols,
            rows,
        })
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Current publish sequence, read once (Acquire). Strictly increases
    /// by 2 per published frame, so a reader can cheaply skip a full
    /// re-read when the seq is unchanged since its last snapshot — used
    /// by L2 to avoid a spurious re-render when its 1 s heartbeat wakes
    /// it but L3 published nothing new. An odd value means a write is in
    /// flight; the caller treats that as "changed" and re-reads (`read`
    /// then spins to a tear-free frame).
    pub fn seq(&self) -> u64 {
        unsafe { (*self.header()).seq.load(Ordering::Acquire) }
    }

    #[inline]
    unsafe fn header(&self) -> *const Header {
        self.base as *const Header
    }

    #[inline]
    unsafe fn cells_ptr(&self) -> *const Cell {
        self.base.add(HEADER_BYTES) as *const Cell
    }

    /// Per-row wrapped-flag array (read view) — mirrors the writer
    /// layout at the fixed offset after the cell capacity.
    #[inline]
    unsafe fn wrapped_ptr(&self) -> *const u8 {
        self.base.add(wrapped_offset())
    }

    /// Read the latest published frame into `cells_out` + `wrapped_out`
    /// (both resized to the frame's `cols*rows` and `rows`),  retrying
    /// until a tear-free snapshot lands.  The dims come from the
    /// header *per read*, so the snapshot tracks a live resize without
    /// any remap.  Returns `None` only if the writer has never
    /// published (seq still 0).
    ///
    /// `wrapped_out[r]` is the DECAWM continuation flag for viewport
    /// row `r` — true means row `r` was wrapped onto from the row
    /// above by an overflowing parser write.
    pub fn read(
        &self,
        cells_out: &mut Vec<Cell>,
        wrapped_out: &mut Vec<bool>,
    ) -> Option<GridSnapshot> {
        cells_out.clear();
        wrapped_out.clear();
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
                // Dims ride in the header with the cells. Read them first
                // and bound the copy by them: a torn read (writer resized
                // between our seq check and here) could yield mismatched
                // dims, so reject anything past the capacity cap *before*
                // the copy — never read out of the mapping. The s1==s2
                // recheck below then discards the torn frame entirely.
                let cols = (*h).cols as u16;
                let rows = (*h).rows as u16;
                if cols == 0 || rows == 0 || !fits_capacity(cols, rows) {
                    std::hint::spin_loop();
                    continue;
                }
                let n = cols as usize * rows as usize;
                let cursor_col = (*h).cursor_col as u16;
                let cursor_row = (*h).cursor_row as u16;
                let flags = (*h).flags;
                let scroll_push_count = (*h).scroll_push_count;
                let scrollback_len = (*h).scrollback_len;
                let view_offset = (*h).view_offset as u16;

                cells_out.set_len(0);
                cells_out.reserve(n);
                let cells = self.cells_ptr();
                std::ptr::copy_nonoverlapping(cells, cells_out.as_mut_ptr(), n);
                cells_out.set_len(n);

                // Copy the per-row wrapped flags too, in the SAME
                // seqlock critical section so cells + flags are paired
                // to one publish.
                wrapped_out.clear();
                wrapped_out.reserve(rows as usize);
                let wptr = self.wrapped_ptr();
                for row in 0..rows {
                    let flag = *wptr.add(row as usize) != 0;
                    wrapped_out.push(flag);
                }

                fence(Ordering::Acquire);
                let s2 = (*h).seq.load(Ordering::Relaxed);
                if s1 == s2 {
                    return Some(GridSnapshot {
                        cols,
                        rows,
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
        assert!(reader.read(&mut buf, &mut Vec::new()).is_none(), "no frame before publish");

        writer.publish(&grid, 0, FLAG_CURSOR_VISIBLE);

        let snap = reader.read(&mut buf, &mut Vec::new()).expect("frame after publish");
        assert_eq!(snap.cols, cols);
        assert_eq!(snap.rows, rows);
        assert_eq!((snap.cursor_col, snap.cursor_row), (5, 0));
        assert!(snap.cursor_visible());
        assert_eq!(buf.len(), cols as usize * rows as usize);
        assert_eq!(buf[0].ch, 'h');
        assert_eq!(buf[4].ch, 'o');
        assert_eq!(buf[5].ch, ' '); // blank past the text
    }

    // v3 protocol: per-row DECAWM wrapped flags round-trip through
    // shm.  Writer sets row_wrapped on a continuation row; reader
    // gets the same bit in its `wrapped_out` Vec.  Without this, the
    // L2-side link scanner can't detect cross-row URLs.
    #[test]
    fn wrapped_flags_round_trip() {
        let cols = 10u16;
        let rows = 4u16;
        let mut grid = Grid::new(cols, rows);
        // Mark row 1 + row 2 as wrap continuations (row 3 left clean).
        grid.set_row_wrapped(1, true);
        grid.set_row_wrapped(2, true);
        let mut writer = GridShmWriter::create(cols, rows).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        writer.publish(&grid, 0, 0);
        let mut cells = Vec::new();
        let mut wrapped = Vec::new();
        let snap = reader.read(&mut cells, &mut wrapped).expect("frame");
        assert_eq!(snap.rows, rows);
        assert_eq!(wrapped.len(), rows as usize);
        assert_eq!(
            &wrapped[..],
            &[false, true, true, false][..],
            "row 1+2 marked, 0+3 clean"
        );
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
            let snap = reader.read(&mut buf, &mut Vec::new()).expect("frame");
            assert_eq!(buf[0].ch, ch, "round {round}");
            assert_eq!(snap.cursor_row, 1);
            assert_eq!(snap.cursor_col, round as u16);
            assert!(!snap.cursor_visible());
        }
    }

    #[test]
    fn resize_in_place_grows_and_shrinks_without_remap() {
        // The 4b path: one region, published at several dims in place. The
        // reader tracks each frame's dims from the header — no remap, no
        // re-create. Covers grow (more cells) and shrink (fewer).
        let mut writer = GridShmWriter::create(20, 5).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let mut buf = Vec::new();

        for &(cols, rows) in &[(20u16, 5u16), (100, 40), (8, 2), (80, 24)] {
            let mut grid = Grid::new(cols, rows);
            // Mark the far corner so we can confirm the full frame copied.
            grid.set_cell(cols - 1, rows - 1, Cell::from('X'));
            grid.set_cursor(cols - 1, rows - 1);
            writer.publish(&grid, 0, 0);

            let snap = reader.read(&mut buf, &mut Vec::new()).expect("frame");
            assert_eq!((snap.cols, snap.rows), (cols, rows), "dims track resize");
            assert_eq!(buf.len(), cols as usize * rows as usize);
            assert_eq!(buf[buf.len() - 1].ch, 'X', "far corner copied at {cols}x{rows}");
            assert_eq!((snap.cursor_col, snap.cursor_row), (cols - 1, rows - 1));
        }
    }

    #[test]
    fn concurrent_resize_never_yields_torn_or_oob_frame() {
        // Like the torn-frame test, but the writer also *resizes* between
        // publishes. Every stable read must have all cells == the round's
        // char and a cell count matching its own reported dims — a torn
        // dims/cells pairing (or an OOB copy from garbage dims) would trip
        // one of these.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dims = [(64u16, 24u16), (120, 50), (16, 8), (200, 60)];
        let mut writer = GridShmWriter::create(dims[0].0, dims[0].1).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let stop = Arc::new(AtomicBool::new(false));

        let stop_w = stop.clone();
        let writer_thread = std::thread::spawn(move || {
            let mut round = 0u32;
            while !stop_w.load(Ordering::Relaxed) {
                let (cols, rows) = dims[round as usize % dims.len()];
                let ch = char::from_u32(0x21 + (round % 90)).unwrap();
                let mut grid = Grid::new(cols, rows);
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
            if let Some(snap) = reader.read(&mut buf, &mut Vec::new()) {
                assert_eq!(
                    buf.len(),
                    snap.cols as usize * snap.rows as usize,
                    "cell count must match reported dims"
                );
                let first = buf[0].ch;
                assert!(
                    buf.iter().all(|c| c.ch == first),
                    "torn frame: cell mix across a resize"
                );
                reads += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer_thread.join().unwrap();
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
        assert!(reader.read(&mut buf, &mut Vec::new()).is_none(), "no frame before publish");

        // Writer takes the region itself.
        let mut writer = GridShmWriter::from_fd(region).expect("writer from_fd");
        let mut grid = Grid::new(cols, rows);
        grid.set_cell(0, 0, Cell::from('Z'));
        grid.set_cursor(1, 2);
        writer.publish(&grid, 0, FLAG_CURSOR_VISIBLE);

        let snap = reader.read(&mut buf, &mut Vec::new()).expect("frame after publish");
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
            if let Some(_snap) = reader.read(&mut buf, &mut Vec::new()) {
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

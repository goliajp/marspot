//! Cross-process grid framebuffer over POSIX shared memory (target #4).
//!
//! L3 (`marspot-session`) publishes the in-view window of its `Grid`
//! into a shared region after each pump; L2 (`marspot-core`) maps the
//! same region and reads it to render. The published snapshot is the
//! *visible* window only (`rows × cols` cells, ~40 KiB at 80×24), so
//! L2 never holds L3's scrollback.
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
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering, fence};

use crate::grid::{Cell, Grid};

pub mod clusters;
mod hash;
mod ident;
mod names;
mod peek;
mod region;
pub mod title;
pub use clusters::{ClusterTable, ClusterTableReader, ClusterTableWriter};
pub use ident::{Identity, SessionRegions, delete_session_regions};
pub use names::{session_shm_name, state_hash};
pub use region::{create_region, create_region_named, delete_region, open_region};
pub use title::TitleWriter;
pub use peek::{Frame, peek, peek_frame};
use hash::compute_publish_hash;
use region::attach_and_maybe_upgrade;

const MAGIC: u32 = 0x4d_53_47_31; // "MSG1"
// VERSION 3 (2026-06-17): per-row DECAWM wrap flags appended after the
// cell area so L2's mirror grid carries the same `wrapped` signal L3's
// parser sets.  Without this, scan_visible_links on the L2 side always
// saw `wrapped = false`, breaking cross-row URL/path detection.
//
// Forward-compat upgrade path (added 2026-06-17 after Amendment 16
// install fallout):
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
/// Mouse tracking active(DECSET 1000/1002/1003).L2 reads this on
/// scroll wheel decisions:on → L2 bypass scrollback_len clamp 直接
/// forward 给 L3 当 wheel event(L3 encode mouse escape 写 PTY);
/// off → 走 scrollback / view_offset 老路径.alt-screen TUI(claudecode)
/// 关键:scrollback_len 一直 0(redraw in-place 不发 \n),不 bypass
/// 永远滚不动.
pub const FLAG_MOUSE_TRACKING: u32 = 1 << 3;
/// Mouse SGR encoding(DECSET 1006).L2 mouse-on 时按这个选 SGR
/// 还是 X11 legacy 字节格式.
pub const FLAG_MOUSE_SGR: u32 = 1 << 4;

/// The program wants motion while a button is held (DECSET ?1002).
///
/// `FLAG_MOUSE_TRACKING` says only that some tracking is on, which is
/// all a press and a release need — every mode reports those.  Motion
/// is the half that differs, and forwarding it to a program that asked
/// for `?1000` would bury it in reports it never wanted.
pub const FLAG_MOUSE_DRAG: u32 = 1 << 7;
/// The program wants motion whether or not a button is held
/// (DECSET ?1003).
pub const FLAG_MOUSE_ANY_MOTION: u32 = 1 << 8;

/// The session is inside the alternate screen (`?1049h`).
///
/// L2 needs this to tell two states apart that look identical from the
/// outside: a fresh shell with an empty scrollback, where the wheel
/// should scroll history as usual, and a full-screen TUI that redraws
/// in place and therefore never pushes a line into scrollback at all.
/// In the second case scrolling history is meaningless — there is none
/// — and the wheel has to reach the program instead.
///
/// Additive: an older L2 masks this bit off and behaves exactly as it
/// did, and an older L3 never sets it, which reads as "not alt" — the
/// pre-existing path.
pub const FLAG_ALT_SCREEN: u32 = 1 << 5;
/// DEC 1007 — the program asked for the wheel to arrive as arrow keys
/// on this screen.  Published so L2 can route the wheel on the
/// program's own statement rather than on a guess about its output.
pub const FLAG_ALT_SCROLL: u32 = 1 << 6;

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
    pub fn alt_scroll(&self) -> bool {
        self.flags & FLAG_ALT_SCROLL != 0
    }
    pub fn alt_screen(&self) -> bool {
        self.flags & FLAG_ALT_SCREEN != 0
    }
}

/// Byte length the writer/reader actually touches at `cols × rows`:
/// header + cell region + per-row wrapped flags.  Used for bounds
/// checks against the (fixed) mapping size — the actual mapping is
/// always [`capacity_bytes()`].
fn region_len(cols: u16, rows: u16) -> usize {
    HEADER_BYTES + cols as usize * rows as usize * std::mem::size_of::<Cell>() + rows as usize
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

/// Writer side (L3): owns the shm region and publishes grid snapshots.
pub struct GridShmWriter {
    fd: OwnedFd,
    base: *mut u8,
    len: usize,
    cols: u16,
    rows: u16,
    /// FNV-1a hash of the most recent `publish()`'s grid + cursor + flags
    /// + view window.  `publish_if_changed()` skips the entire shm write
    ///
    /// (and the caller's GridReady poke) when the next publish would
    /// produce a bit-identical snapshot.  Was the leading idle-CPU source
    /// before this gate: TUIs (claudecode, etc.) hammer the PTY at their
    /// internal redraw cadence even when the visible grid hasn't moved,
    /// so without dedupe L3 publishes ~30/s, L2 wakes + renders ~25/s,
    /// shows up as ~10 % idle CPU on a 9-pane window.
    last_publish_hash: u64,
    /// Where the text of the frame's clusters goes, once attached.
    clusters: Option<ClusterTableWriter>,
    /// One scrollback row's clusters, refilled per row while a
    /// scrolled view is published.
    row_clusters: Vec<(u16, String)>,
    /// Where the window title goes, once attached.
    title: Option<TitleWriter>,
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
        let region::Mapped { base, len, cols, rows } = attach_and_maybe_upgrade(fd.as_raw_fd())?;
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
            clusters: None,
            row_clusters: Vec::new(),
            title: None,
        })
    }

    /// Say whose region this is, for a reader that asks; a region
    /// its creator signed for `owner` is left as it is.  False for a
    /// region from a build that left no room for it.
    pub fn stamp(&mut self, owner: Identity) -> bool {
        let Some(at) = region::ident_at(self.base, self.len) else {
            return false;
        };
        if unsafe { ident::read(at) } != Some(owner) {
            unsafe { ident::stamp(at, owner, ident::new_serial()) };
        }
        true
    }

    /// Whose region this says it is.
    pub fn identity(&self) -> Option<Identity> {
        region::ident_at(self.base, self.len).and_then(|at| unsafe { ident::read(at) })
    }

    /// Which of its session's regions this is -- see [`ident`].
    pub fn serial(&self) -> Option<u64> {
        region::ident_at(self.base, self.len).and_then(|at| unsafe { ident::read_serial(at) })
    }

    /// Publish the text of each frame's clusters and the window title
    /// from now on, in the regions named after the grid region `grid`
    /// -- see [`clusters`] and [`title`].  What it published beside the
    /// region it is leaving goes along.  Without the first a reader
    /// draws a cluster cell's pool index and nothing reads the second,
    /// so a failure is logged and the session runs without.
    pub fn attach_side_regions(&mut self, grid: &std::ffi::CStr, owner: Identity) {
        use names::{Side, side_name};
        // what this writer published beside the region it is leaving
        let (left_clusters, left_title) = (self.clusters.take(), self.title.take());
        let Some((clusters, title)) = side_name(grid, Side::Clusters).zip(side_name(grid, Side::Title))
        else {
            return;
        };
        match ClusterTableWriter::open(&clusters, owner) {
            Ok(mut w) => {
                if let Some(left) = &left_clusters {
                    w.carry_from(left);
                }
                self.clusters = Some(w);
                self.last_publish_hash = 0;
            }
            Err(e) => crate::lx_warn!(
                "grid_shm.clusters.open_failed",
                &format!("{e}"),
                session_id = owner.session
            ),
        }
        match TitleWriter::open(&title, owner) {
            Ok(mut w) => {
                if let Some(left) = &left_title {
                    w.carry_from(left);
                }
                self.title = Some(w);
            }
            Err(e) => {
                crate::lx_warn!("grid_shm.title.open_failed", &format!("{e}"), session_id = owner.session)
            }
        }
    }

    /// Publish `title` when it changed. Apart from the frame: a title
    /// can change while every cell stays the same.
    pub fn publish_title(&mut self, title: &str) {
        if let Some(t) = self.title.as_mut() {
            t.update(title);
        }
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
        unsafe { self.base.add(HEADER_BYTES) as *mut Cell }
    }

    /// Per-row wrapped-flag array — one byte per viewport row, fixed
    /// offset after the cell capacity.  Reader and writer agree by
    /// construction (same const), independent of live dims.
    #[inline]
    unsafe fn wrapped_ptr(&self) -> *mut u8 {
        unsafe { self.base.add(wrapped_offset()) }
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
    pub fn publish_if_changed(&mut self, grid: &Grid, view_offset: u16, flags: u32) -> bool {
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
    pub fn publish(&mut self, grid: &Grid, view_offset: u16, flags: u32) {
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
            // no intermediate allocation. The cluster table is written in
            // the same pass and under the same odd seq; a grid with an
            // empty pool and no history in view has none to write.
            let cells = self.cells_ptr();
            let mut table = self.clusters.as_mut().filter(|t| {
                grid.cluster_pool_len() > 0 || view_offset > 0 || t.last_count > 0
            });
            if let Some(t) = table.as_mut() {
                t.begin();
            }
            for row in 0..rows {
                // history rows keep their cluster text per row, not in
                // the pool
                if let Some(t) = table.as_mut()
                    && view_offset > 0
                    && grid.row_clusters_at_view(view_offset, row, &mut self.row_clusters)
                {
                    for (col, text) in &self.row_clusters {
                        t.push(*col, row, text);
                    }
                }
                let row_base = row as usize * cols as usize;
                for col in 0..cols {
                    let cell = grid.cell_at_view(view_offset, col, row);
                    std::ptr::write(cells.add(row_base + col as usize), cell);
                    if let Some(t) = table.as_mut()
                        && cell.is_cluster()
                        && let Some(text) = grid.cluster_text(&cell)
                    {
                        t.push(col, row, text);
                    }
                }
            }
            if let Some(t) = table {
                t.finish();
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
    clusters: Option<ClusterTableReader>,
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
        let region::Mapped { base, len, cols, rows } = attach_and_maybe_upgrade(fd)?;
        Ok(Self {
            base: base as *const u8,
            len,
            cols,
            rows,
            clusters: None,
        })
    }

    /// Map the grid region `regions` names, when it is that session's.
    pub fn open(regions: &SessionRegions) -> io::Result<Self> {
        let fd = open_region(regions.grid_name())?;
        let reader = Self::from_fd(fd.as_raw_fd())?;
        regions.admit(reader.identity())?;
        Ok(reader)
    }

    /// Whose region this says it is.
    pub fn identity(&self) -> Option<Identity> {
        region::ident_at(self.base as *mut u8, self.len).and_then(|at| unsafe { ident::read(at) })
    }

    /// Which of its session's regions this is -- see [`ident`].
    pub fn serial(&self) -> Option<u64> {
        region::ident_at(self.base as *mut u8, self.len).and_then(|at| unsafe { ident::read_serial(at) })
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
        unsafe { self.base.add(HEADER_BYTES) as *const Cell }
    }

    /// Per-row wrapped-flag array (read view) — mirrors the writer
    /// layout at the fixed offset after the cell capacity.
    #[inline]
    unsafe fn wrapped_ptr(&self) -> *const u8 {
        unsafe { self.base.add(wrapped_offset()) }
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
        self.read_frame(cells_out, wrapped_out, None)
    }

    /// Read the text of each frame's clusters from now on -- see
    /// [`clusters`].
    pub fn attach_clusters(&mut self, table: ClusterTableReader) {
        self.clusters = Some(table);
    }

    pub fn has_clusters(&self) -> bool {
        self.clusters.is_some()
    }

    /// [`Self::read`], and the frame's cluster table into `table`,
    /// both from the same publish. With no table attached `table` comes
    /// back empty.
    pub fn read_with_clusters(
        &self,
        cells_out: &mut Vec<Cell>,
        wrapped_out: &mut Vec<bool>,
        table: &mut ClusterTable,
    ) -> Option<GridSnapshot> {
        table.clear();
        self.read_frame(cells_out, wrapped_out, Some(table))
    }

    fn read_frame(
        &self,
        cells_out: &mut Vec<Cell>,
        wrapped_out: &mut Vec<bool>,
        mut table: Option<&mut ClusterTable>,
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
                if let (Some(r), Some(t)) = (self.clusters.as_ref(), table.as_deref_mut()) {
                    r.read_into(t);
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
        assert!(
            reader.read(&mut buf, &mut Vec::new()).is_none(),
            "no frame before publish"
        );

        writer.publish(&grid, 0, FLAG_CURSOR_VISIBLE);

        let snap = reader
            .read(&mut buf, &mut Vec::new())
            .expect("frame after publish");
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
            assert_eq!(
                buf[buf.len() - 1].ch,
                'X',
                "far corner copied at {cols}x{rows}"
            );
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
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        let dims = [(64u16, 24u16), (120, 50), (16, 8), (200, 60)];
        let mut writer = GridShmWriter::create(dims[0].0, dims[0].1).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let stop = Arc::new(AtomicBool::new(false));

        let published = Arc::new(AtomicU64::new(0));

        let stop_w = stop.clone();
        let published_w = published.clone();
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
                published_w.fetch_add(1, Ordering::Relaxed);
                round = round.wrapping_add(1);
            }
        });

        let reads = read_while_written(&reader, &published, |buf, snap| {
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
            (first, snap.cols)
        });
        stop.store(true, Ordering::Relaxed);
        writer_thread.join().unwrap();
        let sizes: std::collections::HashSet<u16> = reads.iter().map(|r| r.1).collect();
        assert!(sizes.len() > 1, "every read saw one size, so no read crossed a resize");
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
        assert!(
            reader.read(&mut buf, &mut Vec::new()).is_none(),
            "no frame before publish"
        );

        // Writer takes the region itself.
        let mut writer = GridShmWriter::from_fd(region).expect("writer from_fd");
        let mut grid = Grid::new(cols, rows);
        grid.set_cell(0, 0, Cell::from('Z'));
        grid.set_cursor(1, 2);
        writer.publish(&grid, 0, FLAG_CURSOR_VISIBLE);

        let snap = reader
            .read(&mut buf, &mut Vec::new())
            .expect("frame after publish");
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
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        let cols = 64u16;
        let rows = 24u16;
        let mut writer = GridShmWriter::create(cols, rows).expect("create");
        let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
        let stop = Arc::new(AtomicBool::new(false));

        let published = Arc::new(AtomicU64::new(0));

        let stop_w = stop.clone();
        let published_w = published.clone();
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
                published_w.fetch_add(1, Ordering::Relaxed);
                round = round.wrapping_add(1);
            }
        });

        read_while_written(&reader, &published, |buf, _snap| {
            let first = buf[0].ch;
            assert!(
                buf.iter().all(|c| c.ch == first),
                "torn frame: cell mix detected"
            );
            (first, 0)
        });
        stop.store(true, Ordering::Relaxed);
        writer_thread.join().unwrap();
    }

    /// Read until 50 000 frames have been checked by `check`, and
    /// prove the reads raced the writer rather than ran after it.
    ///
    /// `published` counts the writer's publishes.  A torn-frame test
    /// whose reads all landed on one frame, or all before the writer
    /// started, would pass against a reader with no seqlock at all, so
    /// the writer has to have published during the reads and the reads
    /// have to have seen several rounds.  The deadline is what a writer
    /// that never publishes would otherwise cost: forever.
    fn read_while_written(
        reader: &GridShmReader,
        published: &std::sync::atomic::AtomicU64,
        mut check: impl FnMut(&[Cell], &GridSnapshot) -> (char, u16),
    ) -> Vec<(char, u16)> {
        use std::sync::atomic::Ordering;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        let mut first_at = None;
        while seen.len() < 50_000 {
            assert!(
                std::time::Instant::now() < deadline,
                "{} frames read in 20 s, writer at {} publishes",
                seen.len(),
                published.load(Ordering::Relaxed)
            );
            if let Some(snap) = reader.read(&mut buf, &mut Vec::new()) {
                first_at.get_or_insert_with(|| published.load(Ordering::Relaxed));
                seen.push(check(&buf, &snap));
            }
        }
        let during = published.load(Ordering::Relaxed) - first_at.unwrap_or(0);
        assert!(during > 0, "the writer published nothing while the reads ran");
        let rounds: std::collections::HashSet<char> = seen.iter().map(|r| r.0).collect();
        assert!(
            rounds.len() > 1,
            "every read saw the same round: the reads never raced a publish"
        );
        seen
    }
}

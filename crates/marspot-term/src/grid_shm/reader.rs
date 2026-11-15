//! The reading end of a grid region (L2, and anything else that shows a
//! session's screen).

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{Ordering, fence};

use super::cells::FrameBuf;
use super::ident::{self, Identity, SessionRegions};
use super::region::{self, attach_and_maybe_upgrade};
use super::{ClusterTable, ClusterTableReader, GridSnapshot, Header, fits_capacity, open_region, wrapped_offset};
use crate::grid::Cell;

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

    /// The region's version and the layout of the frame in it now, as
    /// the header says them: for tests and diagnostics, not for reading
    /// a frame, which takes the layout under the seqlock.
    pub fn layout(&self) -> (u32, u32) {
        unsafe {
            let h = self.header();
            (std::ptr::read_volatile(&(*h).version), std::ptr::read_volatile(&(*h).cell_size))
        }
    }

    #[inline]
    unsafe fn header(&self) -> *const Header {
        self.base as *const Header
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
    ///
    /// A frame of eight-byte cells comes back expanded; a reader that
    /// can take them as they are uses [`Self::read_into`].
    pub fn read(&self, cells_out: &mut Vec<Cell>, wrapped_out: &mut Vec<bool>) -> Option<GridSnapshot> {
        let mut buf = FrameBuf::default();
        let snap = self.read_frame(&mut buf, wrapped_out, None)?;
        *cells_out = if buf.is_packed { buf.expanded() } else { buf.wide };
        Some(snap)
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
        let mut buf = FrameBuf::default();
        table.clear();
        let snap = self.read_frame(&mut buf, wrapped_out, Some(table))?;
        *cells_out = if buf.is_packed { buf.expanded() } else { buf.wide };
        Some(snap)
    }

    /// The latest frame into `buf`, in whichever layout it was written,
    /// with its wrap flags and the text of its clusters, all from the
    /// same publish.  The buffers keep their capacity from frame to
    /// frame.
    pub fn read_into(
        &self,
        buf: &mut FrameBuf,
        wrapped_out: &mut Vec<bool>,
        table: &mut ClusterTable,
    ) -> Option<GridSnapshot> {
        table.clear();
        self.read_frame(buf, wrapped_out, Some(table))
    }

    fn read_frame(
        &self,
        buf: &mut FrameBuf,
        wrapped_out: &mut Vec<bool>,
        mut table: Option<&mut ClusterTable>,
    ) -> Option<GridSnapshot> {
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
                // the layout is the frame's own: a session switches it
                // when its reader says what it reads
                let cell_size = std::ptr::read_volatile(&(*h).cell_size);
                if !buf.copy_from(self.base, n, cell_size) {
                    std::hint::spin_loop();
                    continue;
                }

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
                    buf.settle();
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

//! Looking at a published frame from outside the pane's processes,
//! read-only: the mode flags, and the frame itself.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{Ordering, fence};

use super::clusters::{ClusterTable, ClusterTableReader};
use super::ident::{self, SessionRegions};
use super::{HEADER_BYTES, Header, MAGIC, fits_capacity, region};
use crate::grid::Cell;

/// Read a published frame's sequence and mode flags without mapping
/// the grid or touching the region.
///
/// For a watcher that only wants to know what the program in the pane
/// has declared about the terminal — alt screen, bracketed paste,
/// mouse tracking.  Deliberately not `GridShmReader`: that maps the
/// whole region RDWR and may perform a one-shot layout upgrade, both
/// of which are too much to ask of a process that is merely looking.
///
/// A region written by an older build is read as far as the header
/// goes, which is all of it: `seq` and `flags` have been at the same
/// offsets since v1.
pub fn peek(regions: &SessionRegions) -> io::Result<(u64, u32)> {
    let map = ReadOnly::of(regions)?;
    let h = map.base as *const Header;
    unsafe {
        if std::ptr::read_volatile(&(*h).magic) != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a marspot grid region"));
        }
        Ok(((*h).seq.load(Ordering::Acquire), std::ptr::read_volatile(&(*h).flags)))
    }
}

/// The grid region a session names, mapped read-only, once it has
/// been found to be that session's.
struct ReadOnly {
    base: *const u8,
    len: usize,
}

impl ReadOnly {
    fn of(regions: &SessionRegions) -> io::Result<Self> {
        let fd = unsafe { libc::shm_open(regions.grid_name().as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let len = region::mapping_len(fd.as_raw_fd())?;
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let map = Self { base: base as *const u8, len };
        let found = region::ident_at(base as *mut u8, len).and_then(|at| unsafe { ident::read(at) });
        regions.admit(found)?;
        Ok(map)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
    }
}

/// One published frame, copied out of the region.
#[derive(Clone, Debug)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    /// How far the viewport is scrolled back. Non-zero means the cells
    /// are history the person is looking at, not the live screen.
    pub view_offset: u16,
    /// `cols * rows`, row-major. A cluster cell holds an index into
    /// the writer's pool; its text is in `clusters`.
    pub cells: Vec<Cell>,
    /// `(col, row, text)` of each cluster in the frame, when the
    /// session publishes them -- see [`super::clusters`].
    pub clusters: Vec<(u16, u16, String)>,
}

/// The frame the pane is showing. `None` when nothing has been
/// published yet.
///
/// This is the screen itself. Replaying the tail of the bytelog into a
/// fresh terminal is not: a full-screen program draws later frames
/// with relative cursor moves against a screen the replay never saw,
/// and Claude Code's input box, plainly on screen, was missing from
/// every such replay of two live panes.
///
/// Mapped read-only, like [`peek`]; one frame is copied under the
/// seqlock.
pub fn peek_frame(regions: &SessionRegions) -> io::Result<Option<Frame>> {
    let map = ReadOnly::of(regions)?;
    // no region is a session from before clusters were published
    let clusters = ClusterTableReader::open(regions).ok();
    unsafe { read_frame(map.base, clusters.as_ref()) }
}

/// # Safety
/// `base` maps at least [`super::capacity_bytes`] of a grid region.
unsafe fn read_frame(
    base: *const u8,
    clusters: Option<&ClusterTableReader>,
) -> io::Result<Option<Frame>> {
    let h = base as *const Header;
    unsafe {
        if std::ptr::read_volatile(&(*h).magic) != MAGIC
            || std::ptr::read_volatile(&(*h).cell_size) as usize != std::mem::size_of::<Cell>()
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a readable grid region"));
        }
    }
    let src = unsafe { base.add(HEADER_BYTES) as *const Cell };
    let mut cells: Vec<Cell> = Vec::new();
    let mut table = ClusterTable::default();
    // same bounded retry as the reader: a writer that died mid-publish
    // leaves seq odd for good
    for _ in 0..1024 {
        let s1 = unsafe { (*h).seq.load(Ordering::Acquire) };
        if s1 == 0 {
            return Ok(None);
        }
        let (cols, rows) = unsafe { ((*h).cols as u16, (*h).rows as u16) };
        if s1 & 1 != 0 || cols == 0 || rows == 0 || !fits_capacity(cols, rows) {
            std::hint::spin_loop();
            continue;
        }
        let (cursor_col, cursor_row, view_offset) = unsafe {
            ((*h).cursor_col as u16, (*h).cursor_row as u16, (*h).view_offset as u16)
        };
        let n = cols as usize * rows as usize;
        cells.clear();
        cells.reserve(n);
        unsafe {
            std::ptr::copy_nonoverlapping(src, cells.as_mut_ptr(), n);
            cells.set_len(n);
        }
        if let Some(r) = clusters {
            r.read_into(&mut table);
        }
        fence(Ordering::Acquire);
        if unsafe { (*h).seq.load(Ordering::Relaxed) } == s1 {
            let clusters = table.iter().map(|(c, r, t)| (c, r, t.to_string())).collect();
            return Ok(Some(Frame { cols, rows, cursor_col, cursor_row, view_offset, cells, clusters }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::{GridShmWriter, create_region_named, session_shm_name};
    use super::*;
    use crate::grid::Grid;

    fn put(grid: &mut Grid, col: u16, row: u16, ch: char) {
        let mut cell = grid.cell(col, row);
        cell.ch = ch;
        grid.set_cell(col, row, cell);
    }

    /// A session no other test or live pane uses.
    fn test_regions(n: u64) -> SessionRegions {
        let id = 0xBE_0000_0000 + u64::from(std::process::id()) * 10 + n;
        SessionRegions::new(session_shm_name(id), id, true)
    }

    #[test]
    fn the_published_frame_reads_back_with_its_cursor() {
        let mut grid = Grid::new(12, 3);
        for col in 0..3 {
            put(&mut grid, col, 0, '\u{2500}');
        }
        put(&mut grid, 0, 1, '\u{276f}');
        // a wide glyph and its spacer
        put(&mut grid, 2, 1, '\u{4e2d}');
        put(&mut grid, 3, 1, '\0');
        let regions = test_regions(0);
        let fd = create_region_named(12, 3, regions.grid_name(), regions.owner()).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        assert!(peek_frame(&regions).unwrap().is_none(), "nothing published yet");
        grid.set_cursor(2, 1);
        w.publish(&grid, 0, 0);
        let f = peek_frame(&regions).unwrap().unwrap();
        regions.delete();
        assert_eq!((f.cols, f.rows, f.cursor_col, f.cursor_row, f.view_offset), (12, 3, 2, 1, 0));
        let row = |r: usize| -> String {
            f.cells[r * 12..(r + 1) * 12].iter().map(|c| c.ch).filter(|&c| c != '\0').collect()
        };
        assert_eq!(row(0).trim_end(), "\u{2500}\u{2500}\u{2500}");
        assert_eq!(row(1).trim_end(), "\u{276f} \u{4e2d}");
    }

    #[test]
    fn a_region_that_does_not_exist_is_an_error() {
        assert!(peek_frame(&test_regions(1)).is_err());
    }

    /// Another app's session has put its region under the name this
    /// session's entry gives: neither its frame nor its flags are read.
    #[test]
    fn the_region_of_another_session_is_not_read() {
        let regions = test_regions(2);
        let theirs = SessionRegions::new(regions.grid_name().to_owned(), regions.owner().session + 1, true);
        let fd = create_region_named(12, 3, theirs.grid_name(), theirs.owner()).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        w.publish(&Grid::new(12, 3), 0, 0);
        let (frame, flags, own) = (peek_frame(&regions), peek(&regions), peek(&theirs));
        regions.delete();
        assert!(frame.is_err(), "another session's frame was read");
        assert!(flags.is_err(), "another session's flags were read");
        assert!(own.is_ok(), "{:?}", own.err());
    }
}

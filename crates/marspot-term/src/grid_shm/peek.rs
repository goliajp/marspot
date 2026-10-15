//! Looking at a published frame from outside the pane's processes,
//! read-only: the mode flags, and the frame itself.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{Ordering, fence};

use super::{HEADER_BYTES, Header, MAGIC, capacity_bytes, fits_capacity};
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
pub fn peek(name: &std::ffi::CStr) -> io::Result<(u64, u32)> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            HEADER_BYTES,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let h = base as *const Header;
    let out = unsafe {
        let magic = std::ptr::read_volatile(&(*h).magic);
        if magic != MAGIC {
            Err(io::Error::new(io::ErrorKind::InvalidData, "not a marspot grid region"))
        } else {
            Ok((
                (*h).seq.load(Ordering::Acquire),
                std::ptr::read_volatile(&(*h).flags),
            ))
        }
    };
    unsafe { libc::munmap(base, HEADER_BYTES) };
    out
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
    /// `cols * rows`, row-major. A cluster cell's text lives in the
    /// writer's pool and is not here.
    pub cells: Vec<Cell>,
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
pub fn peek_frame(name: &std::ffi::CStr) -> io::Result<Option<Frame>> {
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let len = capacity_bytes();
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let out = unsafe { read_frame(base as *const u8) };
    unsafe { libc::munmap(base, len) };
    out
}

/// # Safety
/// `base` maps at least [`capacity_bytes`] of a grid region.
unsafe fn read_frame(base: *const u8) -> io::Result<Option<Frame>> {
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
        fence(Ordering::Acquire);
        if unsafe { (*h).seq.load(Ordering::Relaxed) } == s1 {
            return Ok(Some(Frame { cols, rows, cursor_col, cursor_row, view_offset, cells }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::{GridShmWriter, create_region_named, delete_region};
    use super::*;
    use crate::grid::Grid;

    fn put(grid: &mut Grid, col: u16, row: u16, ch: char) {
        let mut cell = grid.cell(col, row);
        cell.ch = ch;
        grid.set_cell(col, row, cell);
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
        let name = std::ffi::CString::new(format!("/msp-t-pk-{}", std::process::id())).unwrap();
        let fd = create_region_named(12, 3, &name).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        assert!(peek_frame(&name).unwrap().is_none(), "nothing published yet");
        grid.set_cursor(2, 1);
        w.publish(&grid, 0, 0);
        let f = peek_frame(&name).unwrap().unwrap();
        delete_region(&name);
        assert_eq!((f.cols, f.rows, f.cursor_col, f.cursor_row, f.view_offset), (12, 3, 2, 1, 0));
        let row = |r: usize| -> String {
            f.cells[r * 12..(r + 1) * 12].iter().map(|c| c.ch).filter(|&c| c != '\0').collect()
        };
        assert_eq!(row(0).trim_end(), "\u{2500}\u{2500}\u{2500}");
        assert_eq!(row(1).trim_end(), "\u{276f} \u{4e2d}");
    }

    #[test]
    fn a_region_that_does_not_exist_is_an_error() {
        let name = std::ffi::CString::new("/msp-t-pk-none").unwrap();
        assert!(peek_frame(&name).is_err());
    }
}

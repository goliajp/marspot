//! Looking at a published frame from outside the pane's processes,
//! read-only: the mode flags, and the screen as text.

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

/// The published frame as text, one line per row, trailing blanks
/// trimmed. `None` when nothing has been published yet.
///
/// This is the screen the pane is showing. Replaying the tail of the
/// bytelog into a fresh terminal is not: a full-screen program draws
/// later frames with relative cursor moves against a screen the replay
/// never saw, and Claude Code's input box, plainly on screen, was
/// missing from every such replay of two live panes.
///
/// Mapped read-only, like [`peek`]; one frame is copied under the
/// seqlock. A grapheme cluster, whose text lives in the writer's pool,
/// reads as U+FFFD.
pub fn peek_text(name: &std::ffi::CStr) -> io::Result<Option<String>> {
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
    let out = unsafe { read_text(base as *const u8) };
    unsafe { libc::munmap(base, len) };
    out
}

/// # Safety
/// `base` maps at least [`capacity_bytes`] of a grid region.
unsafe fn read_text(base: *const u8) -> io::Result<Option<String>> {
    let h = base as *const Header;
    unsafe {
        if std::ptr::read_volatile(&(*h).magic) != MAGIC
            || std::ptr::read_volatile(&(*h).cell_size) as usize != std::mem::size_of::<Cell>()
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a readable grid region"));
        }
    }
    let cells = unsafe { base.add(HEADER_BYTES) as *const Cell };
    let mut copy: Vec<Cell> = Vec::new();
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
        let n = cols as usize * rows as usize;
        copy.clear();
        copy.reserve(n);
        unsafe {
            std::ptr::copy_nonoverlapping(cells, copy.as_mut_ptr(), n);
            copy.set_len(n);
        }
        fence(Ordering::Acquire);
        if unsafe { (*h).seq.load(Ordering::Relaxed) } == s1 {
            return Ok(Some(rows_text(&copy, cols)));
        }
    }
    Ok(None)
}

fn rows_text(cells: &[Cell], cols: u16) -> String {
    let mut out = String::new();
    for row in cells.chunks(cols as usize) {
        let start = out.len();
        for c in row {
            match c.ch {
                '\0' => {}
                _ if c.is_cluster() => out.push('\u{fffd}'),
                ch => out.push(ch),
            }
        }
        out.truncate(start + out[start..].trim_end().len());
        out.push('\n');
    }
    out
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
    fn the_published_frame_reads_back_as_text() {
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
        assert_eq!(peek_text(&name).unwrap(), None, "nothing published yet");
        w.publish(&grid, 0, 0);
        let text = peek_text(&name).unwrap().unwrap();
        delete_region(&name);
        assert_eq!(text, "\u{2500}\u{2500}\u{2500}\n\u{276f} \u{4e2d}\n\n");
    }

    #[test]
    fn a_region_that_does_not_exist_is_an_error() {
        let name = std::ffi::CString::new("/msp-t-pk-none").unwrap();
        assert!(peek_text(&name).is_err());
    }
}

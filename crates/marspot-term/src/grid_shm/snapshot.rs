//! What one published frame says besides its cells, and which of the
//! core's view asks it answers.
//!
//! The header's view word carries the offset in its low half and, in
//! its high half, the number of the last scroll or prompt jump the
//! session applied.  A reader from before the number reads the low
//! half alone; a session from before it writes 0 there.

use super::{
    FLAG_ALT_SCREEN, FLAG_ALT_SCROLL, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE, FLAG_CURSOR_VISIBLE, GridShmWriter,
};

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
    /// The last view ask the session applied before this frame; 0 from
    /// a session that does not count them.
    pub view_seq: u16,
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

pub(super) fn view_word(view_offset: u16, view_seq: u16) -> u32 {
    view_offset as u32 | (view_seq as u32) << 16
}

/// The offset and the ask number in a header's view word.
pub(super) fn split_view_word(word: u32) -> (u16, u16) {
    (word as u16, (word >> 16) as u16)
}

impl GridShmWriter {
    /// The session applied the core's view ask `seq`: frames from now
    /// on say so.  The next publish goes out even with the same cells,
    /// since the core waits for it.  True when `seq` is new.
    pub fn set_view_seq(&mut self, seq: u16) -> bool {
        if seq == self.view_seq {
            return false;
        }
        self.view_seq = seq;
        self.last_publish_hash = 0;
        true
    }

    /// The last ask [`Self::set_view_seq`] recorded, or the one the
    /// region's previous writer published.
    pub fn view_seq(&self) -> u16 {
        self.view_seq
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, OwnedFd};

    use super::*;
    use crate::grid::Grid;
    use crate::grid_shm::{ClusterTable, FrameBuf, GridShmReader};

    #[test]
    fn the_view_word_holds_both_halves() {
        assert_eq!(split_view_word(view_word(0xBEEF, 0x1234)), (0xBEEF, 0x1234));
        assert_eq!(view_word(7, 0), 7, "a session that does not count writes what it always did");
    }

    /// The frame after an ask carries it even when nothing on screen
    /// changed, and the next writer on the region picks it up.
    #[test]
    fn an_applied_ask_is_published_and_survives_a_new_writer() {
        let mut w = GridShmWriter::create(10, 3).unwrap();
        let grid = Grid::new(10, 3);
        assert!(w.publish_if_changed(&grid, 0, 0));
        assert!(!w.publish_if_changed(&grid, 0, 0));
        assert!(w.set_view_seq(5));
        assert!(!w.set_view_seq(5));
        assert!(w.publish_if_changed(&grid, 0, 0), "the answer to the ask was not published");
        let r = GridShmReader::from_fd(unsafe { libc::dup(w.fd()) }).unwrap();
        let snap = r.read_into(&mut FrameBuf::default(), &mut Vec::new(), &mut ClusterTable::default()).unwrap();
        assert_eq!((snap.view_offset, snap.view_seq), (0, 5));
        let next = GridShmWriter::from_fd(unsafe { OwnedFd::from_raw_fd(libc::dup(w.fd())) }).unwrap();
        assert_eq!(next.view_seq(), 5);
        assert_eq!(next.published_view_offset(), 0);
    }
}

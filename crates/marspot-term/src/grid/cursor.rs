//! The cursor, and how far up and down it has been.

use super::Grid;

/// The rows the cursor went up to and from, numbered absolutely: the
/// scroll-push count plus the row on screen.  A cursor that only goes
/// down writes between where it was and where it is; one that goes up
/// and down again has written outside that too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reach {
    pub top: u64,
    pub bottom: u64,
}

impl Reach {
    /// The rows of both.
    pub fn join(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) => Some(Self { top: a.top.min(b.top), bottom: a.bottom.max(b.bottom) }),
            (a, b) => a.or(b),
        }
    }
}

impl Grid {
    pub fn cursor(&self) -> (u16, u16) {
        (self.cursor_col, self.cursor_row)
    }

    /// Clamp-and-set the cursor.  Out-of-bounds values clamp to the last
    /// valid position; the cursor is always within `[0, cols) x [0, rows)`.
    /// For a cursor that may go up, [`Self::move_cursor`].
    pub fn set_cursor(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols - 1);
        self.cursor_row = row.min(self.rows - 1);
    }

    /// [`Self::set_cursor`], noting the rows gone up from and to.
    pub fn move_cursor(&mut self, col: u16, row: u16) {
        if row < self.cursor_row {
            let pushed = self.scroll_push_count();
            let moved = Reach { top: pushed + u64::from(row), bottom: pushed + u64::from(self.cursor_row) };
            self.reach = Reach::join(self.reach, Some(moved));
        }
        self.set_cursor(col, row);
    }

    /// How far the cursor has gone up since the last ask.
    pub fn take_reach(&mut self) -> Option<Reach> {
        self.reach.take()
    }
}

#[cfg(test)]
mod tests {
    use crate::grid::Reach;
    use crate::terminal::Terminal;

    #[test]
    fn a_cursor_that_goes_up_and_down_again_tells_how_far() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"a\r\nb\r\nc\r\nd\r\ne");
        assert_eq!(t.take_cursor_reach(), None, "only down");
        let ups: [&[u8]; 5] = [b"\x1b[H", b"\x1b[3A", b"\x1b[3F", b"\x1b[1d", b"\x1bM\x1bM\x1bM"];
        for up in ups {
            t.feed(b"\x1b[4;1H");
            t.take_cursor_reach();
            t.feed(up);
            t.feed(b"x\r\ny");
            assert_eq!(t.take_cursor_reach(), Some(Reach { top: 1, bottom: 4 }), "{up:?}");
            assert_eq!(t.take_cursor_reach(), None, "asked");
        }
        t.feed(b"\x1b[1;1H\x1b7\x1b[4;1H");
        t.take_cursor_reach();
        t.feed(b"\x1b8");
        assert_eq!(t.take_cursor_reach(), Some(Reach { top: 1, bottom: 4 }), "a cursor put back");
    }

    #[test]
    fn the_alternate_screen_s_cursor_is_not_the_main_one_s() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"a\r\nb\r\nc\r\nd\r\ne\x1b[?1049h\x1b[Hx\r\ny");
        assert_eq!(t.main_cursor(), (4, 1));
        assert_eq!(t.take_cursor_reach(), None);
        t.feed(b"\x1b[Hx\x1b[?1049l");
        assert_eq!(t.main_cursor(), (4, 1));
        assert_eq!(t.take_cursor_reach(), None);
    }
}

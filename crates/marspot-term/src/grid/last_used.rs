//! How far down a screen goes before only blank rows are left.

use super::{Cell, Color, Grid};

/// Whether a cell paints anything.
fn shows(cell: &Cell) -> bool {
    !matches!(cell.ch, ' ' | '\0') || cell.attrs.bg != Color::DEFAULT || cell.attrs.reverse || cell.attrs.underline
}

impl Grid {
    /// The last row of the live screen that shows something or holds
    /// the cursor.
    pub fn last_used_row(&self) -> u16 {
        let cursor = self.cursor().1;
        (cursor + 1..self.rows()).rev().find(|&r| (0..self.cols()).any(|c| shows(&self.cell(c, r)))).unwrap_or(cursor)
    }
}

#[cfg(test)]
mod helper_strip_tests {
    use crate::terminal::Terminal;

    fn last(bytes: &[u8]) -> u16 {
        let mut t = Terminal::new(20, 10);
        t.feed(bytes);
        t.grid().last_used_row()
    }

    #[test]
    fn the_last_row_is_the_lowest_of_the_text_and_the_cursor() {
        assert_eq!(last(b""), 0);
        assert_eq!(last(b"a\r\nb\r\n"), 2, "the cursor is under the text");
        assert_eq!(last(b"\x1b[7;1Hx\x1b[2;1H"), 6, "the text is under the cursor");
        assert_eq!(last(b"\x1b[10;20Hx\x1b[H"), 9, "the last cell of the screen");
        assert_eq!(last(b"\x1b[2;1Hx\x1b[H"), 1, "right under the cursor");
    }

    #[test]
    fn a_row_that_is_only_shaded_is_used() {
        assert_eq!(last(b"\x1b[5;1H\x1b[44m \x1b[0m\x1b[H"), 4);
        assert_eq!(last(b"\x1b[5;1H\x1b[7m \x1b[0m\x1b[H"), 4);
        assert_eq!(last(b"\x1b[5;1Hx\x08 \x1b[H"), 0, "a blanked cell shows nothing");
    }
}

//! Finding a text watch's text on the grid: rows numbered absolutely
//! (see `track`), soft-wrapped rows read as the one line they were,
//! compared cell by cell against the text decoded once, with no string
//! built for a line.

use marspot_term::grid::Grid;

use crate::track::At;

/// Most rows one batch's check reads, newest first.  A batch that
/// scrolled more than this past is checked on the rows nearest the
/// cursor.
pub(crate) const BATCH_ROWS: u64 = 4096;
/// Most rows a check at registration reads.
pub(crate) const REGISTER_ROWS: u64 = 20_000;
/// Most rows looked back over for the start of a line.
const JOIN_ROWS: u64 = 256;
/// A watch's text is at most 256 bytes, so at most 256 characters.
const MAX_CHARS: usize = 256;

#[derive(Clone, Copy)]
enum Loc {
    Screen(u16),
    History(usize),
}

fn locate(grid: &Grid, abs: u64) -> Option<Loc> {
    let pushed = grid.scroll_push_count();
    if abs >= pushed {
        let row = abs - pushed;
        return (row < u64::from(grid.rows())).then_some(Loc::Screen(row as u16));
    }
    let back = usize::try_from(pushed - abs).ok()?;
    let len = grid.scrollback_len();
    (back <= len).then(|| Loc::History(len - back))
}

/// The row continues the line of the row above it.
fn continues(grid: &Grid, loc: Loc) -> bool {
    match loc {
        Loc::Screen(r) => grid.row_wrapped(r),
        Loc::History(i) => grid.scrollback_wrapped(i),
    }
}

/// A watch's text as characters, with the table that lets a compare
/// go on from a partial match without reading a cell twice.
pub(crate) struct Needle {
    chars: [char; MAX_CHARS],
    /// For a match of the first `i + 1` characters that fails next: how
    /// many of them still match as a prefix.
    back: [u16; MAX_CHARS],
    len: usize,
}

impl Needle {
    /// `None` for empty text; text over 256 bytes is refused before.
    pub(crate) fn new(text: &str) -> Option<Box<Self>> {
        let mut n = Box::new(Self { chars: ['\0'; MAX_CHARS], back: [0; MAX_CHARS], len: 0 });
        for c in text.chars().take(MAX_CHARS) {
            n.chars[n.len] = c;
            n.len += 1;
        }
        if n.len == 0 {
            return None;
        }
        let mut k = 0;
        for i in 1..n.len {
            while k > 0 && n.chars[i] != n.chars[k] {
                k = usize::from(n.back[k - 1]);
            }
            if n.chars[i] == n.chars[k] {
                k += 1;
            }
            n.back[i] = k as u16;
        }
        Some(n)
    }

    /// Take one character of the line; whether the text ends with it.
    #[inline]
    fn step(&self, state: &mut usize, c: char) -> bool {
        let mut s = *state;
        while s > 0 && self.chars[s] != c {
            s = usize::from(self.back[s - 1]);
        }
        if self.chars[s] == c {
            s += 1;
        }
        if s == self.len {
            *state = usize::from(self.back[s - 1]);
            return true;
        }
        *state = s;
        false
    }
}

/// Take one cell; whether the text ends with it.
#[inline]
fn feed_cell(grid: &Grid, n: &Needle, state: &mut usize, ch: char, cluster: Option<u32>) -> bool {
    match cluster {
        None if ch == '\0' => false,
        None => n.step(state, ch),
        Some(i) => match grid.cluster_text_at(i) {
            Some(text) => text.chars().fold(false, |e, c| n.step(state, c) | e),
            // a stale index reads as the blank it will be overwritten with
            None => n.step(state, ' '),
        },
    }
}

/// Feed one row's cells from `from_col` on; whether the text ended in
/// them.  `'\0'` is the right half of a wide character, which is no
/// character.
fn feed_row(grid: &Grid, loc: Loc, from_col: u16, n: &Needle, state: &mut usize) -> bool {
    match loc {
        Loc::Screen(r) => {
            let Some(cells) = grid.live_row_at_view(0, r) else { return false };
            for cell in cells.iter().skip(usize::from(from_col)) {
                if feed_cell(grid, n, state, cell.ch(), cell.cluster_index()) {
                    return true;
                }
            }
        }
        Loc::History(i) => {
            for col in from_col..grid.cols() {
                let Some(cell) = grid.scrollback_cell(i, col) else { break };
                if feed_cell(grid, n, state, cell.ch, cell.cluster_index()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Where a check reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    /// Where the output after the watch's position started.  What the
    /// grid had before it on this cell's line is not read: the cells
    /// before it on its row, and the rows its line wrapped from, which
    /// hold a command's own echo however often the shell redraws it.
    pub(crate) floor: At,
    /// The rows read, each with the rows its line wrapped from.
    pub(crate) lo: u64,
    pub(crate) hi: u64,
}

/// The first row of the line `abs` is on, looking back at most
/// `JOIN_ROWS`, and not above a row `stop` says starts it.
fn line_top(grid: &Grid, mut abs: u64, stop: impl Fn(u64) -> bool) -> u64 {
    for _ in 0..JOIN_ROWS {
        let wrapped = locate(grid, abs).is_some_and(|l| continues(grid, l));
        if !wrapped || abs == 0 || locate(grid, abs - 1).is_none() || stop(abs) {
            break;
        }
        abs -= 1;
    }
    abs
}

/// The first row of the first line in `span` that holds `n`, reading at
/// most `limit` rows, the newest; and whether rows were left unread.
pub(crate) fn find(grid: &Grid, n: &Needle, span: Span, limit: u64) -> (Option<u64>, bool) {
    let Span { floor, hi, .. } = span;
    let newest = hi.saturating_sub(limit);
    let cut = span.lo < newest;
    let floor_top = line_top(grid, floor.0, |_| false);
    let before_floor = |abs: u64| abs >= floor_top && abs < floor.0;
    let mut abs = line_top(grid, span.lo.max(newest), |abs| abs == floor.0 || before_floor(abs - 1));
    let mut state = 0;
    let mut start = abs;
    let first = abs;
    while abs <= hi {
        let loc = locate(grid, abs).filter(|_| !before_floor(abs));
        let Some(loc) = loc else {
            abs += 1;
            state = 0;
            continue;
        };
        if abs == first || abs == floor.0 || !continues(grid, loc) {
            state = 0;
            start = abs;
        }
        let from_col = if abs == floor.0 { floor.1 } else { 0 };
        if feed_row(grid, loc, from_col, n, &mut state) {
            return (Some(start), cut);
        }
        abs += 1;
    }
    (None, cut)
}

/// The whole screen of `grid`, for a full-screen program's alternate
/// screen, which has no history to number from.
pub(crate) fn find_on_screen(grid: &Grid, n: &Needle) -> Option<u64> {
    let top = grid.scroll_push_count();
    find(grid, n, Span { floor: (top, 0), lo: top, hi: top + u64::from(grid.rows()) - 1 }, u64::MAX).0
}

/// What the byte stream is searched for before the grid is: the text
/// without the spaces at its ends, which the grid also gets from cursor
/// moves and tabs, as bytes.  `None` for text that is all spaces, which
/// every batch is checked for.
pub(crate) fn stream_text(text: &str) -> Option<&[u8]> {
    let t = text.trim_matches(' ');
    (!t.is_empty()).then_some(t.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::terminal::Terminal;

    fn term(cols: u16, rows: u16, bytes: &str) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        t.feed(bytes.as_bytes());
        t
    }

    fn rows(lo: u64, hi: u64) -> Span {
        Span { floor: (lo, 0), lo, hi }
    }

    fn found(t: &Terminal, text: &str, span: Span) -> Option<u64> {
        find(t.grid(), &Needle::new(text).unwrap(), span, 100).0
    }

    #[test]
    fn watch_text_a_soft_wrapped_line_matches_as_one() {
        let t = term(10, 4, "0123456789abcdef\r\nnext");
        assert_eq!(found(&t, "789abc", rows(0, 2)), Some(0));
        let second = Span { floor: (0, 0), lo: 1, hi: 1 };
        assert_eq!(found(&t, "789abc", second), Some(0), "found from its second row");
        assert_eq!(found(&t, "789abc", rows(2, 2)), None, "not on the next line");
    }

    #[test]
    fn watch_text_a_line_in_the_history_keeps_its_number() {
        let t = term(10, 3, "first\r\nsecond\r\nthird\r\nfourth\r\nfifth");
        assert_eq!(found(&t, "second", rows(0, 10)), Some(1));
        assert_eq!(found(&t, "fifth", rows(0, 10)), Some(4));
        assert_eq!(found(&t, "second", rows(2, 10)), None);
    }

    #[test]
    fn watch_text_cells_before_the_floor_are_not_read() {
        let t = term(20, 4, "$ run; echo DONE\r\nDONE\r\n");
        let echo = Span { floor: (0, 16), lo: 0, hi: 0 };
        assert_eq!(found(&t, "DONE", echo), None, "the command's own echo");
        let after = Span { floor: (0, 16), lo: 0, hi: 2 };
        assert_eq!(found(&t, "DONE", after), Some(1));
        let mid = Span { floor: (0, 12), lo: 0, hi: 0 };
        assert_eq!(found(&t, "DONE", mid), Some(0), "written at the floor: counts");
    }

    #[test]
    fn watch_text_the_rows_a_long_command_wrapped_from_are_not_read() {
        // the command line wraps from row 0 to row 1; the floor is its end
        let t = term(10, 5, "$ x; DONE; y\r\nDONE\r\n");
        let floor = (1, 2);
        assert_eq!(found(&t, "DONE", Span { floor, lo: 0, hi: 1 }), None, "the echo, however the rows are asked");
        assert_eq!(found(&t, "DONE", Span { floor, lo: 0, hi: 3 }), Some(2));
    }

    #[test]
    fn watch_text_rows_above_the_floor_are_read_when_asked() {
        // a clear: new output on rows above where the watch started
        let t = term(20, 4, "fresh DONE\r\n\r\n$ ");
        assert_eq!(found(&t, "DONE", Span { floor: (2, 2), lo: 0, hi: 2 }), Some(0));
    }

    #[test]
    fn watch_text_wide_characters_and_partial_matches() {
        let t = term(20, 3, "漢字テスト aab aaab\r\n");
        assert_eq!(found(&t, "字テ", rows(0, 1)), Some(0), "the right halves are not characters");
        assert_eq!(found(&t, "aaab", rows(0, 1)), Some(0));
        assert_eq!(found(&t, "aaaa", rows(0, 1)), None);
        let t = term(20, 3, "e\u{301}x\r\n");
        assert_eq!(found(&t, "e\u{301}x", rows(0, 1)), Some(0), "a combining mark is part of its cell");
    }

    #[test]
    fn watch_text_the_stream_text_drops_the_end_spaces() {
        assert_eq!(stream_text(" test result: "), Some(&b"test result:"[..]));
        assert_eq!(stream_text("   "), None);
    }
}

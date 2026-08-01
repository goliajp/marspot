//! Reading a pane — the other half of talking to one.
//!
//! Nothing has to be subscribed to, buffered or streamed: L3 already
//! appends every byte a pane's PTY produces to that session's bytelog,
//! so the record is on disk, complete, and someone else's problem to
//! keep bounded.  Reading is therefore a thing you do *when you need
//! to*, not a pipe you hold open.
//!
//! What comes back is the pane as a person would see it, not the raw
//! stream.  Raw output is escape sequences, cursor moves and repaints —
//! a `claude` pane rewrites the same rows hundreds of times a second,
//! so "the last 4 KB of output" is meaningless while "what is on the
//! screen" is exactly the question.  The way to turn one into the other
//! is a terminal emulator, and marspot is one: replay the tail through
//! a headless [`Terminal`] of the pane's own size and read the grid.
//!
//! The replay is an approximation in one direction only — state set
//! before the window (a colour, a mode) is missing, never invented.  In
//! practice a full-screen program repaints far more often than the
//! window is long, and a shell's output is plain enough not to care.

use marspot_term::terminal::Terminal;

/// How much of the tail to replay.
///
/// Big enough to hold several full repaints of a large window (a
/// 200×50 pane repainting with colour runs ~40 KB), small enough that
/// reading a pane is a millisecond.  The floor on usefulness is a
/// program that draws once and then sits there — with less tail than
/// its last full paint, the top of the screen comes back blank.
pub const REPLAY_TAIL: u64 = 512 * 1024;

/// What a pane's screen says right now.
///
/// `extra_lines` asks for that many lines of what has already scrolled
/// off the top, which the replay still has: 0 gives the visible screen.
pub fn screen_text(
    bytelog: &std::path::Path,
    cols: u16,
    rows: u16,
    extra_lines: u16,
) -> std::io::Result<String> {
    let bytes = tail(bytelog, REPLAY_TAIL)?;
    let mut term = Terminal::new(cols.max(1), rows.max(1));
    term.feed(&bytes);
    Ok(render(&term, extra_lines))
}

/// The last `n` bytes of a file, or all of it when it is shorter.
fn tail(path: &std::path::Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // Start at a byte boundary we chose, not one that splits a UTF-8
    // sequence or an escape: the parser resynchronises within a few
    // bytes either way, and the alternative — scanning backwards for a
    // safe point — is work for no visible gain.
    f.seek(SeekFrom::Start(len.saturating_sub(n)))?;
    let mut buf = Vec::with_capacity(n.min(len) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The grid as text: `extra_lines` of scrollback, then the screen,
/// with trailing blank lines dropped.
fn render(term: &Terminal, extra_lines: u16) -> String {
    let grid = term.grid();
    let mut out: Vec<String> = Vec::new();
    let history = grid.scrollback_len().min(extra_lines as usize);
    for i in (0..history).rev() {
        out.push(scrollback_line(grid, i));
    }
    for row in 0..grid.rows() {
        out.push(row_text(grid, |c| grid.cell(c, row).ch));
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// One line of scrollback, `back` rows above the screen.
fn scrollback_line(grid: &marspot_term::grid::Grid, back: usize) -> String {
    // `cell_at_view` counts rows up from the live screen, so a
    // viewport row of 0 with an offset of `back + 1` is the line that
    // scrolled off `back` rows ago.
    let off = (back + 1).min(u16::MAX as usize) as u16;
    row_text(grid, |c| grid.cell_at_view(off, c, 0).ch)
}

/// One row as text, minus the grid's own bookkeeping.
///
/// A wide character occupies two cells and the second holds NUL as a
/// sentinel — it is a *layout* fact, not a character.  Collected
/// verbatim it lands in the middle of every CJK word: `继\0续`, which
/// a terminal swallows into a space so the damage is invisible until
/// something tries to match on the text.  And something does: the
/// autorun policy reads this.
fn row_text(grid: &marspot_term::grid::Grid, cell: impl Fn(u16) -> char) -> String {
    (0..grid.cols())
        .map(cell)
        .filter(|ch| *ch != '\0')
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "marspot-read-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// The point of replaying rather than tailing: what comes back is
    /// the screen, not the keystrokes that produced it.
    #[test]
    fn a_repainted_screen_reads_as_what_is_on_it_now() {
        // Draw "first", then move home and overwrite with "second" —
        // exactly what a full-screen program does on every frame.  A
        // raw tail would show both; a replay shows what a person sees.
        let p = write(
            "repaint",
            b"first line\r\nsecond line\r\n\x1b[H\x1b[2JAFTER REPAINT\r\n",
        );
        let text = screen_text(&p, 40, 6, 0).unwrap();
        assert_eq!(text, "AFTER REPAINT");
        assert!(!text.contains("first line"), "the erased frame is gone");
        let _ = std::fs::remove_file(p);
    }

    /// Blank rows below the content are not content.
    #[test]
    fn trailing_blank_rows_are_dropped() {
        let p = write("blanks", b"one\r\ntwo\r\n");
        assert_eq!(screen_text(&p, 20, 24, 0).unwrap(), "one\ntwo");
        let _ = std::fs::remove_file(p);
    }

    /// Asking for scrollback returns what scrolled off, oldest first,
    /// above the screen.
    #[test]
    fn extra_lines_reach_above_the_screen() {
        let mut b = Vec::new();
        for i in 1..=10 {
            b.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        let p = write("scroll", &b);
        // A 3-row window: lines 8, 9 are on screen (10's newline put
        // the cursor on a blank last row).
        let screen = screen_text(&p, 20, 3, 0).unwrap();
        assert!(screen.contains("line 10"), "got {screen:?}");
        assert!(!screen.contains("line 7"), "line 7 has scrolled off: {screen:?}");

        let with_history = screen_text(&p, 20, 3, 4).unwrap();
        assert!(with_history.contains("line 6"), "got {with_history:?}");
        // Oldest first: history sits above the screen, in reading order.
        let six = with_history.find("line 6").unwrap();
        let ten = with_history.find("line 10").unwrap();
        assert!(six < ten, "history must come first: {with_history:?}");
        let _ = std::fs::remove_file(p);
    }

    /// A wide character occupies two cells; the second is a sentinel,
    /// not a character.
    ///
    /// Collected verbatim it lands inside every CJK word (`继\0续`),
    /// which a terminal swallows into a space — invisible until
    /// something matches on the text, and the autorun policy does.
    #[test]
    fn wide_characters_do_not_leave_a_hole_in_the_text() {
        let p = write("wide", "可以 /clear 了\r\n继续 autorun\r\n".as_bytes());
        let text = screen_text(&p, 40, 4, 0).unwrap();
        assert_eq!(text, "可以 /clear 了\n继续 autorun");
        assert!(!text.contains('\0'), "no sentinels in what a caller reads");
        let _ = std::fs::remove_file(p);
    }

    /// A pane that has said nothing reads as nothing — not an error.
    #[test]
    fn an_empty_pane_reads_as_empty() {
        let p = write("empty", b"");
        assert_eq!(screen_text(&p, 40, 10, 0).unwrap(), "");
        let _ = std::fs::remove_file(p);
    }

    /// A missing bytelog is an error the caller should see, not an
    /// empty screen that looks like a quiet pane.
    #[test]
    fn a_missing_bytelog_is_an_error() {
        let p = std::env::temp_dir().join("marspot-read-nothing-here");
        let _ = std::fs::remove_file(&p);
        assert!(screen_text(&p, 40, 10, 0).is_err());
    }

    /// Only the tail is read, however long the log has grown — reading
    /// a pane must cost the same on day one and day thirty.
    #[test]
    fn only_the_tail_is_read() {
        let mut b = vec![b'x'; (REPLAY_TAIL as usize) * 2];
        b.extend_from_slice(b"\x1b[H\x1b[2Jthe end\r\n");
        let p = write("huge", &b);
        let text = screen_text(&p, 20, 4, 0).unwrap();
        assert_eq!(text, "the end");
        let _ = std::fs::remove_file(p);
    }
}

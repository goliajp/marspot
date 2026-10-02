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

/// What has been typed into an agent's input box and not sent, if
/// anything.
///
/// Read off the grid rather than its text, because the text cannot
/// tell three things apart that the person can: messages already sent
/// carry the same prompt glyph as the box (and sit above it), Claude
/// Code's agent list can put one below it, and the program's own
/// suggestion or placeholder is drawn *in* the box. So the box is the
/// prompt row that holds the cursor -- or the one a wrapped line
/// continues from -- and dim cells in it are the program's, not the
/// person's.
///
/// Only the prompt row is returned; a line wrapped past it is cut at
/// the wrap.
pub fn composer_text(
    bytelog: &std::path::Path,
    cols: u16,
    rows: u16,
) -> std::io::Result<Option<String>> {
    let bytes = tail(bytelog, REPLAY_TAIL)?;
    let mut term = Terminal::new(cols.max(1), rows.max(1));
    term.feed(&bytes);
    Ok(composer(term.grid()))
}

/// Claude Code draws U+276F, Codex U+203A.
const PROMPTS: [char; 2] = ['\u{276f}', '\u{203a}'];
/// The box's own sides, when it is drawn as a box.
const SIDES: [char; 3] = ['│', '┃', '|'];

fn composer(grid: &marspot_term::grid::Grid) -> Option<String> {
    let (_, cursor_row) = grid.cursor();
    let all = |r: u16| row_text_of(grid, r, 0, |c| grid.cell(c, r), |_| true);
    // Up from the cursor to the prompt it is typing after. A blank row
    // or a rule on the way means the cursor is not in the box at all,
    // and not knowing has to read as nothing: pasting a guess into
    // someone's input is worse than losing a line they can retype.
    let row = (0..=cursor_row).rev().find_map(|r| {
        let line = all(r);
        let t = line.trim().trim_start_matches(SIDES).trim_start();
        if t.starts_with(PROMPTS) {
            Some(Some(r))
        } else if t.is_empty() || t.chars().all(|c| matches!(c, '─' | '━' | '╭' | '╮' | '╰' | '╯')) {
            Some(None)
        } else {
            None
        }
    })??;
    let typed = row_text_of(grid, row, 0, |c| grid.cell(c, row), |cell| !cell.attrs.dim);
    let t = typed.trim().trim_start_matches(SIDES).trim_start();
    let rest = t.strip_prefix(PROMPTS)?.trim().trim_end_matches(SIDES).trim();
    (!rest.is_empty()).then(|| rest.to_string())
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
        out.push(row_text_of(grid, row, 0, |c| grid.cell(c, row), |_| true));
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
    row_text_of(grid, 0, off, |c| grid.cell_at_view(off, c, 0), |_| true)
}

/// One row as text, minus the grid's own bookkeeping.
///
/// A wide character occupies two cells and the second holds NUL as a
/// sentinel — it is a *layout* fact, not a character.  Collected
/// verbatim it lands in the middle of every CJK word: `继\0续`, which
/// a terminal swallows into a space so the damage is invisible until
/// something tries to match on the text.  And something does: the
/// autorun policy reads this.
/// One row as text.
///
/// Takes the cell rather than its `ch`, because a cell holding a
/// grapheme cluster keeps the text in the grid's pool and its `ch` is
/// the pool index — printing that would put a plane-15 codepoint in
/// what a person reads.  Cells `keep` turns down read as blanks.
fn row_text_of(
    grid: &marspot_term::grid::Grid,
    viewport_row: u16,
    view_offset: u16,
    cell: impl Fn(u16) -> marspot_term::grid::Cell,
    keep: impl Fn(&marspot_term::grid::Cell) -> bool,
) -> String {
    let mut out = String::with_capacity(grid.cols() as usize);
    let mut row_clusters = Vec::new();
    grid.row_clusters_at_view(view_offset, viewport_row, &mut row_clusters);
    for c in 0..grid.cols() {
        let cell = cell(c);
        // NUL is the wide glyph's trailing half.
        if cell.ch == '\0' {
            continue;
        }
        if !keep(&cell) {
            out.push(' ');
            continue;
        }
        match grid
            .cluster_text(&cell)
            .or_else(|| marspot_term::grid::cluster_in_row(&row_clusters, c))
        {
            Some(text) => out.push_str(text),
            None => out.push(cell.ch),
        }
    }
    out.trim_end().to_string()
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

    /// A Claude Code screen as it draws one: a sent message above,
    /// the box between two rules, the agent list below, and the
    /// cursor parked in the box.
    fn claude(box_row: &str) -> Vec<u8> {
        let rule = "─".repeat(40);
        format!(
            "\x1b[1;1H\x1b[48;2;55;55;55m\x1b[38;2;80;80;80m❯ \x1b[38;2;255;255;255msent before\x1b[39m\x1b[49m\
             \x1b[3;1H{rule}\x1b[4;1H{box_row}\x1b[5;1H{rule}\
             \x1b[7;3H⏺ main\x1b[8;1H❯ ◯ general-purpose\
             \x1b[4;3H"
        )
        .into_bytes()
    }

    fn composer_of(name: &str, bytes: &[u8]) -> Option<String> {
        let p = write(name, bytes);
        let got = composer_text(&p, 40, 10).unwrap();
        let _ = std::fs::remove_file(p);
        got
    }

    /// The grey suggestion Claude Code draws in an empty box is not
    /// something anyone typed.  Read as text it was, and a profile
    /// switch pasted it back in as if it were.
    #[test]
    fn a_suggestion_drawn_in_the_box_is_not_unsent_text() {
        let bytes = claude("\x1b[39m❯ \x1b[2m继续 autorun\x1b[22m");
        assert_eq!(composer_of("ghost", &bytes), None);
    }

    /// What the person typed is read, and only up to where the
    /// program's own completion starts.
    #[test]
    fn typed_text_is_read_and_the_completion_after_it_is_not() {
        let bytes = claude("\x1b[39m❯ half a thought");
        assert_eq!(composer_of("typed", &bytes).as_deref(), Some("half a thought"));
        let bytes = claude("\x1b[39m❯ /com\x1b[2mpact\x1b[22m");
        assert_eq!(composer_of("completion", &bytes).as_deref(), Some("/com"));
    }

    /// A message already sent and the agent list both carry the same
    /// glyph as the box; neither is the box.
    #[test]
    fn sent_messages_and_the_agent_list_are_not_the_box() {
        let bytes = claude("\x1b[39m❯");
        assert_eq!(composer_of("empty", &bytes), None);
    }

    /// Codex: the conversation's first message is at the top of the
    /// screen with the box's glyph, and the empty box shows a dim
    /// placeholder.  Taking the first prompt on screen carried that
    /// old message across a switch.
    #[test]
    fn codex_reads_its_box_not_the_first_message_on_screen() {
        let screen = |box_text: &str| {
            format!(
                "\x1b[1;1H\x1b[1m\x1b[2m\x1b[39;48;2;41;42;43m› \x1b[22m\x1b[22m滚动一格 10% 固定值吧\x1b[49m\
                 \x1b[8;1H\x1b[1m\x1b[39;48;2;31;32;33m›\x1b[8;3H\x1b[22m{box_text}\x1b[49m\
                 \x1b[10;3HGPT-6-Astra high\x1b[8;3H"
            )
            .into_bytes()
        };
        let placeholder = screen("\x1b[2mAsk Codex to do anything\x1b[22m");
        assert_eq!(composer_of("codex-empty", &placeholder), None);
        let typed = screen("还没发的");
        assert_eq!(composer_of("codex-typed", &typed).as_deref(), Some("还没发的"));
    }

    /// A line long enough to wrap leaves the cursor on the row below
    /// the prompt; the box is still the prompt it continues from.
    #[test]
    fn a_wrapped_line_is_read_from_its_prompt_row() {
        let bytes = format!(
            "\x1b[3;1H{rule}\x1b[4;1H❯ the first part\x1b[5;3Hand more\x1b[6;1H{rule}\x1b[5;11H",
            rule = "─".repeat(40)
        );
        assert_eq!(
            composer_of("wrapped", bytes.as_bytes()).as_deref(),
            Some("the first part")
        );
    }

    /// The cursor somewhere other than a box -- mid-repaint, a dialog
    /// -- reads as nothing, not as the nearest prompt above it.
    #[test]
    fn a_cursor_outside_any_box_reads_as_nothing() {
        let bytes = b"\x1b[1;1H\xe2\x9d\xaf sent before\x1b[3;1Houtput\x1b[5;1H";
        assert_eq!(composer_of("outside", bytes), None);
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

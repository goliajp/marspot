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
/// tell apart things the person can: messages already sent carry the
/// same prompt glyph as the box, Claude Code's agent list can put one
/// below it, and the program's own suggestion or placeholder is drawn
/// *in* the box.  So the box is found from the cursor, dim cells in it
/// are the program's, and every row of it is read, not just the first.
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

fn is_rule(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| matches!(c, '─' | '━' | '╭' | '╮' | '╰' | '╯'))
}

/// One cell of the box as typed: its column, its width, its text.
type Glyph = (u16, u16, String);

/// Not knowing reads as nothing throughout: pasting a guess into
/// someone's input is worse than losing a line they can retype.
fn composer(grid: &marspot_term::grid::Grid) -> Option<String> {
    use marspot_term::grid::Color;
    let bg = |r: u16| grid.cell(0, r).attrs.bg;
    let bare = |r: u16| {
        let line = row_text_of(grid, r, 0, |c| grid.cell(c, r));
        line.trim().trim_start_matches(SIDES).trim_start().to_string()
    };
    // Up from the cursor to the prompt it is typing after.  A rule or a
    // change of shade on the way means the cursor is not in the box.
    let (_, cursor_row) = grid.cursor();
    let mut row = cursor_row;
    while !bare(row).starts_with(PROMPTS) {
        if row == 0 || is_rule(&bare(row)) || bg(row) != bg(cursor_row) {
            return None;
        }
        row -= 1;
    }
    let glyph_col = (0..grid.cols()).find(|&c| PROMPTS.contains(&grid.cell(c, row).ch))?;
    let codex = grid.cell(glyph_col, row).ch == '\u{203a}';
    // Claude Code rules the box off above; Codex shades it.  A prompt
    // glyph without either is a message drawn in the transcript.
    let boxed = if codex { bg(row) != Color::DEFAULT } else { row > 0 && is_rule(&bare(row - 1)) };
    if !boxed {
        return None;
    }
    // Down to the box's end: the rule under it, or where the shade
    // stops.  Blank rows before that are the person's own blank lines.
    let end = (row + 1..grid.rows()).find(|&r| is_rule(&bare(r)) || bg(r) != bg(row))?;
    let lines: Vec<Vec<Glyph>> = (row..end).map(|r| typed_glyphs(grid, r, glyph_col + 2)).collect();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str(joint(&lines[i - 1], line, grid.cols(), codex));
        }
        line.iter().for_each(|g| out.push_str(&g.2));
    }
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// A row of the box from `from` on, dim cells blanked, without the
/// blanks and box side that pad it out to the right.
fn typed_glyphs(grid: &marspot_term::grid::Grid, row: u16, from: u16) -> Vec<Glyph> {
    let mut clusters = Vec::new();
    grid.row_clusters_at_view(0, row, &mut clusters);
    let mut out: Vec<Glyph> = Vec::new();
    for c in from..grid.cols() {
        let cell = grid.cell(c, row);
        if cell.ch == '\0' {
            continue;
        }
        let wide = c + 1 < grid.cols() && grid.cell(c + 1, row).ch == '\0';
        let text = if cell.attrs.dim {
            " ".to_string()
        } else {
            match grid
                .cluster_text(&cell)
                .or_else(|| marspot_term::grid::cluster_in_row(&clusters, c))
            {
                Some(t) => t.to_string(),
                None => cell.ch.to_string(),
            }
        };
        out.push((c, if wide { 2 } else { 1 }, text));
    }
    let trailing_blank = |o: &mut Vec<Glyph>| {
        while o.last().is_some_and(|g| g.2.trim().is_empty()) {
            o.pop();
        }
    };
    trailing_blank(&mut out);
    if out.last().is_some_and(|g| g.2.chars().all(|ch| SIDES.contains(&ch))) {
        out.pop();
        trailing_blank(&mut out);
    }
    out
}

/// What stood between two rows of the box before it was drawn.
///
/// The box wraps a long line by itself, so a row break is either the
/// person's newline or the program's wrap, and the screen does not say
/// which.  The wrap does leave a mark: it only moves text down that
/// would not have fit.  So if the next row's first word fits after the
/// previous row, the break was the person's.  Words are what each
/// program wraps by -- Claude Code only at spaces, so a run of Chinese
/// is one word; Codex between any two wide characters too.  A wrap at
/// a space ate the space; between two wide characters there was none.
fn joint(prev: &[Glyph], next: &[Glyph], cols: u16, codex: bool) -> &'static str {
    let (Some(last), Some(first)) = (prev.last(), next.first()) else {
        return "\n";
    };
    // A wrap never starts a row with a space: the space is what it
    // broke at.  Leading blanks are the person's indentation.
    if first.2 == " " {
        return "\n";
    }
    let both_wide = last.1 == 2 && first.1 == 2;
    let word: u16 = if codex && first.1 == 2 {
        2
    } else {
        next.iter()
            .take_while(|g| g.2 != " " && !(codex && g.1 == 2))
            .map(|g| g.1)
            .sum()
    };
    let gap = if codex && both_wide { 0 } else { 1 };
    // Codex never writes the last column: over the panes measured its
    // rows stop at 72 of 73, Claude Code's reach 73.
    let width = if codex { cols - 1 } else { cols };
    if last.0 + last.1 + gap + word <= width {
        "\n"
    } else if both_wide {
        ""
    } else {
        " "
    }
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
        out.push(row_text_of(grid, row, 0, |c| grid.cell(c, row)));
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
    row_text_of(grid, 0, off, |c| grid.cell_at_view(off, c, 0))
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
/// what a person reads.
fn row_text_of(
    grid: &marspot_term::grid::Grid,
    viewport_row: u16,
    view_offset: u16,
    cell: impl Fn(u16) -> marspot_term::grid::Cell,
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

    /// A Claude Code box at the real panes' width, holding `rows`,
    /// with the cursor after the last of them.
    fn claude_box(rows: &[&str]) -> Vec<u8> {
        let rule = "─".repeat(73);
        let mut b = format!("\x1b[2;1H{rule}");
        for (i, r) in rows.iter().enumerate() {
            b.push_str(&format!("\x1b[{};1H{r}", i + 3));
        }
        b.push_str(&format!("\x1b[{};1H{rule}\x1b[{};{}H", rows.len() + 3, rows.len() + 2, 60));
        b.into_bytes()
    }

    fn composer_at_73(name: &str, bytes: &[u8]) -> Option<String> {
        let p = write(name, bytes);
        let got = composer_text(&p, 73, 20).unwrap();
        let _ = std::fs::remove_file(p);
        got
    }

    /// The whole box travels, not its first row.  These rows are a
    /// real message as Claude Code wrapped it: only at spaces, so a
    /// row can end early when the run of Chinese after it is long.
    #[test]
    fn a_wrapped_message_is_read_whole() {
        let bytes = claude_box(&[
            "❯ 以前我们一直开发效率挺高的，搞这些以后效率其实越来越差了，insight home",
            "  是没办法，add device 也是因为需要大量",
            "  mock/seed，否则绝大多数情况下，特别是 insight，我们都是 staging / prod",
            "  开发",
        ]);
        assert_eq!(
            composer_at_73("claude-wrap", &bytes).as_deref(),
            Some(
                "以前我们一直开发效率挺高的，搞这些以后效率其实越来越差了，insight home \
                 是没办法，add device 也是因为需要大量 \
                 mock/seed，否则绝大多数情况下，特别是 insight，我们都是 staging / prod 开发"
            )
        );
        // Ended at 28 columns, yet a wrap: the next word is 48 wide.
        let bytes = claude_box(&[
            "❯ 我们现在不只是时间，token",
            "  也花了太多在测试上，所有项目都如此，所以才会有刚刚 devops 的全局要求",
        ]);
        assert_eq!(
            composer_at_73("claude-short-wrap", &bytes).as_deref(),
            Some("我们现在不只是时间，token 也花了太多在测试上，所有项目都如此，所以才会有刚刚 devops 的全局要求")
        );
    }

    /// The person's own line breaks and blank lines survive, and so
    /// does their indentation.
    #[test]
    fn the_persons_line_breaks_are_kept() {
        let bytes = claude_box(&["❯ first line", "  second", "", "    indented"]);
        assert_eq!(
            composer_at_73("breaks", &bytes).as_deref(),
            Some("first line\nsecond\n\n  indented")
        );
    }

    /// Codex wraps between any two wide characters, with nothing
    /// between them to lose.  Real rows, and a real fenced paste whose
    /// newline has to survive.
    #[test]
    fn codex_rows_join_the_way_codex_wrapped_them() {
        let shaded = |rows: &[&str]| {
            let mut b = String::from("\x1b[48;2;31;32;33m");
            for (i, r) in rows.iter().enumerate() {
                b.push_str(&format!("\x1b[{};1H{r}", i + 3));
            }
            b.push_str(&format!("\x1b[49m\x1b[{};1H\x1b[2K\x1b[{};60H", rows.len() + 3, rows.len() + 2));
            b.into_bytes()
        };
        let bytes = shaded(&[
            "› content 部分宽度大一点，然后 logo 什么的不行，我们在这里最开始要做的就",
            "  是产品 vis，你先把上层的企业/lab vis 好好读一下，应该要继承的做好，在",
            "  ~/workspace/stable/goliajp/vis 里",
        ]);
        assert_eq!(
            composer_at_73("codex-wrap", &bytes).as_deref(),
            Some(
                "content 部分宽度大一点，然后 logo 什么的不行，我们在这里最开始要做的就\
                 是产品 vis，你先把上层的企业/lab vis 好好读一下，应该要继承的做好，在 \
                 ~/workspace/stable/goliajp/vis 里"
            )
        );
        // Ends at 71 with a wide character next: one column short,
        // because Codex leaves the last one empty.
        let bytes = shaded(&[
            "› 画布要支持 zoomin/out，按住 alt用鼠标滚轮可以放大缩小，里面需要的元素",
            "  都要变",
        ]);
        assert_eq!(
            composer_at_73("codex-edge", &bytes).as_deref(),
            Some("画布要支持 zoomin/out，按住 alt用鼠标滚轮可以放大缩小，里面需要的元素都要变")
        );
        let bytes = shaded(&["› ```", "  今天凌晨，OpenAI在旧金山举行DevDay 2026", "  ```"]);
        assert_eq!(
            composer_at_73("codex-fence", &bytes).as_deref(),
            Some("```\n今天凌晨，OpenAI在旧金山举行DevDay 2026\n```")
        );
    }

    /// A delivered message in Claude Code's transcript has the Codex
    /// glyph and no shade; it is not a box, whatever is under it.
    #[test]
    fn a_delivered_message_in_the_transcript_is_not_a_box() {
        let bytes = "\x1b[1;1H\x1b[38;2;153;153;153m› Message from @devops-0d: hi\x1b[39m\
                     \x1b[2;3Hmore of it\x1b[2;14H"
            .as_bytes();
        assert_eq!(composer_of("delivered", bytes), None);
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

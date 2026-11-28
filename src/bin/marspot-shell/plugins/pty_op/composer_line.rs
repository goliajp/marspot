//! Reading the composer line off a pane's screen.

/// Is `line` still sitting on the pane's prompt, unsent?
///
/// claude's interface marks both the composer and every message
/// already sent with the same glyph, so the question is not "is the
/// text on screen" — after a successful submit it is, in the
/// transcript.  It is "is it on the LAST prompt", which is the
/// composer.  That is the rule that told the five panes of
/// 2026-09-30 apart by hand, and it is the one used here.
/// The characters an agent's composer begins its prompt line with.
///
/// Read off the real panes rather than guessed: Claude Code draws
/// U+276F, Codex draws U+203A, and a plain shell draws `>`. The list
/// being short is the point -- when none of them is on screen this
/// function says so rather than inventing an answer.
const COMPOSER_PROMPTS: [char; 3] = ['\u{276f}', '\u{203a}', '>'];

/// Text that is sitting in the composer because something delivered it
/// there, not because the person typed it.
///
/// Another session's message arrives by being written into the input
/// box, so it is indistinguishable from a half-finished sentence by
/// shape alone. Carrying it across a switch puts someone else's words
/// back under the person's cursor, and the carry writes it in again on
/// every switch after that.
pub(super) fn delivered_not_typed(t: &str) -> bool {
    t.trim_start()
        .strip_prefix("Message from @")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(from, _)| !from.is_empty() && !from.contains(char::is_whitespace))
}

fn is_composer_line(l: &str) -> bool {
    let t = l.trim_start();
    COMPOSER_PROMPTS.iter().any(|p| t.starts_with(*p))
}

/// Is the line we pasted still sitting unsent on the prompt?
///
/// **A screen this cannot read counts as "still there".** It used to
/// count as "gone", which made every way of failing look like success:
/// a pane mid-repaint, a prompt character we do not know, a composer
/// scrolled out of view -- each returned `false`, the submit was
/// logged `landed`, and the text stayed in the box. On 2026-10-01
/// three panes sat with an unsent line while the log said every one
/// of them had landed, and `.stuck` -- the branch that gives up and
/// says so -- had never once fired in the whole file.
///
/// Not knowing is not the same as knowing it worked. Saying "still
/// there" costs another return, and the retry ceiling turns a wrong
/// guess into a reported failure instead of a silent one.
pub(super) fn line_is_still_in_the_composer(screen: &str, line: &str) -> bool {
    let needle = std::str::from_utf8(submit_needle(line)).unwrap_or(line);
    match screen.lines().rev().find(|l| is_composer_line(l)) {
        Some(last_prompt) => last_prompt.contains(needle),
        None => true,
    }
}

/// The piece of a submitted line to look for coming back.
///
/// Short, and taken from the front: the composer wraps a long line, so
/// the further in a fragment sits the likelier a line break lands in
/// the middle of it.  A prefix that survives wrapping is enough —
/// nothing else in the pane is going to be painting it.
pub(super) fn submit_needle(line: &str) -> &[u8] {
    let mut end = line.len().min(12);
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    &line.as_bytes()[..end]
}

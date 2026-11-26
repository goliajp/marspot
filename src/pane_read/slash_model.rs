//! Whether what Claude Code's input box would send is its `/model`
//! command, which writes the default model of every account sharing
//! the settings file.

use super::input_box::{box_at, box_of};
use marspot_term::grid::Grid;

/// Claude Code's prompt glyph; Codex and Kimi draw others.
const CLAUDE_PROMPT: char = '\u{276f}';

/// The most rows above the cursor the prompt is looked for in: a box
/// taller than this is a long message, and `/model` is one line.
const LOOK_UP: u16 = 40;

/// The row of Claude Code's prompt above the cursor, if the cursor is
/// in Claude Code's input box.  Reads cells only: this runs on Enter in
/// every pane, before anything is known about the program in it.
pub fn claude_prompt_row(grid: &Grid) -> Option<u16> {
    let (_, cursor_row) = grid.cursor();
    let lowest = cursor_row.saturating_sub(LOOK_UP);
    (lowest..=cursor_row).rev().find(|&row| {
        let first = (0..grid.cols())
            .map(|c| grid.cell(c, row).ch)
            .find(|&ch| !matches!(ch, ' ' | '\0' | '│' | '┃' | '|'));
        first == Some(CLAUDE_PROMPT)
    })
}

/// What Claude Code's input box holds, `Some("")` for an empty one;
/// None when the cursor is not in Claude Code's box.
pub fn claude_box_text(grid: &Grid) -> Option<String> {
    let row = claude_prompt_row(grid)?;
    box_at(grid).filter(|b| b.prompt_row == row)?;
    box_of(grid)
}

/// Whether `line`, sent from the input box by Enter or Tab, runs
/// `/model`: `/model` with or without arguments, or a start of it at
/// least three long on its own (`/mo`, `/mod`, `/mode`), which Claude
/// Code's completion list runs as `/model` when that is the entry it
/// has highlighted.
pub fn is_model_command(line: &str) -> bool {
    let line = line.trim_start();
    let (head, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    head == "/model" || (rest.trim().is_empty() && head.len() >= 3 && "/model".starts_with(head))
}

/// What a program's input does to Claude Code's box.
#[derive(Debug, PartialEq)]
pub struct Typed {
    /// It would run `/model`.
    pub sends: bool,
    /// What the box holds after it, as far as it can be followed.
    pub left: String,
    /// Whether a byte that cannot be followed went in and no Enter of
    /// the caller's has sent the box since: `left` may then not be what
    /// the box shows.
    pub lost: bool,
}

/// Whether Claude Code would run `/model` if a program's `typed` went
/// into the box now holding `held`, followed by an Enter when `enter`.
pub fn sends_model(held: &str, typed: &str, paste: bool, enter: bool) -> bool {
    typed_into(held, false, typed, paste, enter).sends
}

/// What a program's `typed` does to the box now holding `held`, an
/// Enter after it when `enter`; `lost` carries [`Typed::lost`] of the
/// input that left `held`.
///
/// A paste goes into the box whole, its line breaks with it, and only
/// the Enter after it sends.  Typed text sends at every line break and
/// leaves the box empty after each, and a Tab on a line that is
/// `/model` runs it from the completion list; backspace, Ctrl-U and
/// Ctrl-W edit the line as they would.  Any other control byte or escape
/// sequence (Ctrl-A, Home, cursor keys, a bracketed paste of its own)
/// cannot be followed: after one, a send of a line holding a `/` counts
/// as sending `/model`, as does a send with `/model` shown anywhere.
pub fn typed_into(held: &str, lost: bool, typed: &str, paste: bool, enter: bool) -> Typed {
    let runs = |line: &str, lost: bool| is_model_command(line) || (lost && line.contains('/'));
    if paste {
        let whole = format!("{held}{typed}");
        let sends = enter && runs(&whole, lost);
        return if enter { Typed { sends, left: String::new(), lost: false } } else { Typed { sends, left: whole, lost } };
    }
    let mut line = held.to_string();
    let (mut sent, mut control, mut lost) = (false, false, lost);
    for c in typed.chars() {
        match c {
            '\r' | '\n' if runs(&line, lost) => return Typed { sends: true, left: line, lost },
            '\r' | '\n' => (sent, line) = (true, String::new()),
            '\t' if is_model_command(&line) => return Typed { sends: true, left: line, lost },
            '\x7f' | '\x08' => {
                line.pop();
            }
            '\x15' => line.clear(),
            '\x17' => line.truncate(line.trim_end().trim_end_matches(|c: char| !c.is_whitespace()).len()),
            c if c.is_control() => lost = true,
            c => line.push(c),
        }
        control |= c.is_control() && !matches!(c, '\r' | '\n');
    }
    let shown: String = held.chars().chain(typed.chars()).filter(|c| !c.is_control()).collect();
    let sends = (enter && runs(&line, lost)) || ((sent || enter) && control && shown.contains("/model"));
    if enter { Typed { sends, left: String::new(), lost: false } } else { Typed { sends, left: line, lost } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::terminal::Terminal;

    fn screen(bytes: &str) -> Terminal {
        let mut t = Terminal::new(40, 8);
        t.feed(bytes.as_bytes());
        t
    }

    fn claude_box(typed: &str) -> Terminal {
        let rule = "\u{2500}".repeat(40);
        screen(&format!("{rule}\r\n\u{276f} {typed}\r\n{rule}\r\n  ? for shortcuts\x1b[2;{}H", 3 + typed.chars().count()))
    }

    #[test]
    fn model_with_or_without_arguments_is_the_command_and_its_neighbours_are_not() {
        for line in ["/model", "/model opus", "  /model\tsonnet", "/model "] {
            assert!(is_model_command(line), "{line:?}");
        }
        for line in ["/models", "/modelx", "model", "say /model", "", "/m", "/", "/mcp", "/mo opus", "/memory"] {
            assert!(!is_model_command(line), "{line:?}");
        }
    }

    #[test]
    fn a_start_of_model_the_completion_list_would_run_counts_as_model() {
        for line in ["/mo", "/mod", "/mode", " /mod "] {
            assert!(is_model_command(line), "{line:?}");
        }
        assert!(sends_model("", "/mod\r", false, false));
        assert!(sends_model("/mo", "", false, true));
        assert!(sends_model("", "/mo\t", false, false), "Tab runs the highlighted entry");
        assert!(!sends_model("", "/help\t", false, false));
    }

    #[test]
    fn after_a_byte_that_cannot_be_followed_a_send_of_a_slash_is_refused() {
        assert!(sends_model("", "del\x01/mo\r", false, false), "Ctrl-A moved the cursor to the start");
        assert!(sends_model("", "del\x1b[H/mo\r", false, false), "Home");
        assert!(sends_model("", "del\x01/mo", false, true), "with the Enter after it");
        assert!(!sends_model("", "del\x01mo\r", false, false), "no slash, nothing refused");
        let t = typed_into("", false, "del\x01/mo", false, false);
        assert!(!t.sends && t.lost, "{t:?}");
        assert!(typed_into(&t.left, t.lost, "\r", false, false).sends, "the next input carries what was lost");
        assert!(!typed_into("del/mo", false, "\r", false, false).sends);
    }

    #[test]
    fn typed_text_sends_model_from_any_line_with_the_box_empty_after_each_break() {
        assert!(sends_model("", "/model fable\r", false, false));
        assert!(sends_model("/mod", "el\n", false, false));
        assert!(sends_model("", "/model", false, true), "an Enter after it");
        assert!(!sends_model("", "/model", false, false), "typing it alone sends nothing");
        assert!(!sends_model("hello ", "/model\r", false, false));
        assert!(sends_model("", "hi\r/model\r", false, false), "the box is empty after the first line");
        assert!(sends_model("draft", "\r/model opus", false, true));
        assert!(!sends_model("", "hi\r/models\r", false, false));
    }

    #[test]
    fn line_editing_bytes_are_followed_and_other_control_bytes_refuse_model() {
        assert!(sends_model("hello ", "\x15/model\r", false, false), "Ctrl-U empties the box");
        assert!(sends_model("abc", "\x7f\x7f\x7f/model\r", false, false), "backspaces empty it");
        assert!(sends_model("/modelx", "\x08\r", false, false), "a backspace leaves /model");
        assert!(sends_model("say /model", "\x17\x17\r", false, false), "an edit with /model shown and a send");
        assert!(sends_model("say ", "\x17/model\r", false, false), "Ctrl-W takes the word");
        assert!(sends_model("", "\x1b[200~/model\x1b[201~\r", false, false), "a bracketed paste of its own");
        assert!(sends_model("", "/model\x1b[D\r", false, false), "a cursor key");
        assert!(sends_model("", "/model\x1b[D", false, true), "an escape and the Enter after");
        assert!(!sends_model("", "\x1b[200~say hi\x1b[201~\r", false, false), "no /model, nothing refused");
        assert!(!sends_model("", "\x1b[200~/model\x1b[201~", false, false), "no send, nothing refused");
    }

    #[test]
    fn a_paste_is_sent_whole_only_by_the_enter_after_it() {
        assert!(sends_model("", "/model fable", true, true));
        assert!(sends_model("", "/model\nmore", true, true), "a break inside a paste does not send it");
        assert!(!sends_model("", "say\n/model", true, true), "the box starts with say");
        assert!(!sends_model("", "/model fable", true, false));
    }

    #[test]
    fn claude_codes_box_is_told_from_a_shell_and_from_codexs() {
        let t = claude_box("/model opus");
        assert_eq!(claude_prompt_row(t.grid()), Some(1));
        assert_eq!(claude_box_text(t.grid()).as_deref(), Some("/model opus"));
        assert_eq!(claude_box_text(claude_box("").grid()).as_deref(), Some(""));
        let shell = screen("$ /model opus");
        assert_eq!(claude_prompt_row(shell.grid()), None);
        assert_eq!(claude_box_text(shell.grid()), None);
        let codex = screen("\x1b[48;5;236m\u{203a} /model\x1b[K\r\n\x1b[K\x1b[0m\r\n\x1b[1;3H");
        assert_eq!(claude_box_text(codex.grid()), None);
        // a prompt glyph in the transcript with no rule over it is a sent message
        let sent = screen("\u{276f} /model opus\r\n\r\n");
        assert_eq!(claude_box_text(sent.grid()), None);
    }
}

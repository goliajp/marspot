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

/// Whether `line`, sent from the input box, is `/model` with or without
/// arguments.
pub fn is_model_command(line: &str) -> bool {
    line.trim_start().strip_prefix("/model").is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Whether Claude Code would run `/model` if a program's `typed` went
/// into the box now holding `held`, followed by an Enter when `enter`.
///
/// A paste goes into the box whole, its line breaks with it, and only
/// the Enter after it sends.  Typed text sends at every line break and
/// leaves the box empty after each; backspace, Ctrl-U and Ctrl-W edit
/// the line as they would.  Any other control byte or escape sequence
/// (cursor keys, a bracketed paste of its own) cannot be followed, so
/// text holding one, `/model` in the box or in the text, and a send
/// counts as sending it.
pub fn sends_model(held: &str, typed: &str, paste: bool, enter: bool) -> bool {
    if paste {
        return enter && is_model_command(&format!("{held}{typed}"));
    }
    let mut line = held.to_string();
    let (mut sent, mut control) = (false, false);
    for c in typed.chars() {
        match c {
            '\r' | '\n' => {
                if is_model_command(&line) {
                    return true;
                }
                (sent, line) = (true, String::new());
            }
            '\x7f' | '\x08' => {
                line.pop();
            }
            '\x15' => line.clear(),
            '\x17' => line.truncate(line.trim_end().trim_end_matches(|c: char| !c.is_whitespace()).len()),
            c if c.is_control() => {}
            c => line.push(c),
        }
        control |= c.is_control() && !matches!(c, '\r' | '\n');
    }
    if enter && is_model_command(&line) {
        return true;
    }
    let shown: String = held.chars().chain(typed.chars()).filter(|c| !c.is_control()).collect();
    (sent || enter) && control && shown.contains("/model")
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
        for line in ["/models", "/modelx", "model", "say /model", "", "/mode"] {
            assert!(!is_model_command(line), "{line:?}");
        }
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

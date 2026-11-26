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

/// Whether Claude Code would run `/model` if `typed` were typed into
/// the box now holding `held` and then sent: `typed` up to its first
/// line break, after what the box holds.
pub fn sends_model(held: &str, typed: &str) -> bool {
    let typed = typed.split(['\r', '\n']).next().unwrap_or("");
    let mut line = String::with_capacity(held.len() + typed.len());
    line.push_str(held);
    line.push_str(typed);
    is_model_command(&line)
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
        assert!(sends_model("", "/model fable\r"));
        assert!(sends_model("/mod", "el\n"));
        assert!(!sends_model("hello ", "/model"));
        assert!(!sends_model("", "ok\r/model\r"), "only the first line is sent from this box");
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

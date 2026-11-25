//! What a gemini pane's screen says that its session file does not:
//! the quota dialog waiting for an answer, a tool waiting for approval,
//! text typed into the input box and not sent, and a turn still running.
//!
//! The screen is gemini's interface as drawn, so only its bottom rows
//! count: what scrolled into the history above is over.

use super::quota::{LIMIT, unboxed};

/// The rows a dialog's limit line may sit above its last option row.
const DIALOG_ROWS: usize = 14;
/// The rows that may follow a dialog's last option: the box's bottom
/// and gemini's footer.
const BELOW_ROWS: usize = 4;
/// The rows at the bottom an approval prompt or the input box is
/// looked for in.
const BOTTOM_ROWS: usize = 24;

/// What gemini shows in an empty input box.
const PLACEHOLDER: &str = "Type your message or @path/to/file";

/// The screen from the quota dialog's limit line down, when the screen
/// ends with that dialog: numbered option rows under the limit line,
/// and no input box below them.  A limit line left in the history, with
/// the prompt back under it, is not a dialog.
pub(super) fn bottom_dialog(screen: &str) -> Option<&str> {
    let at = screen.rfind(LIMIT)?;
    let tail = &screen[screen[..at].rfind('\n').map_or(0, |i| i + 1)..];
    let rows: Vec<&str> = tail.lines().collect();
    let last = rows.iter().rposition(|r| is_option(r))?;
    let below = &rows[last + 1..];
    let fits = last <= DIALOG_ROWS && below.len() <= BELOW_ROWS;
    (fits && !below.iter().any(|r| prompt_text(r).is_some())).then_some(tail)
}

/// `● 1. Switch to …` or `  2. Stop`: a row of a list gemini asks the
/// user to pick from.
fn is_option(row: &str) -> bool {
    let t = unboxed(row);
    let t = t.strip_prefix('●').unwrap_or(t).trim_start();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && t[digits..].starts_with(". ")
}

fn bottom(screen: &str) -> impl Iterator<Item = &str> {
    let n = screen.lines().count();
    screen.lines().skip(n.saturating_sub(BOTTOM_ROWS))
}

/// gemini asks whether a tool may run: its approval list is up.
pub(super) fn asks_approval(screen: &str) -> bool {
    const ROWS: [&str; 4] = ["Allow once", "Allow for this session", "No, suggest changes", "Waiting for user confirmation"];
    bottom(screen).any(|r| ROWS.iter().any(|w| r.contains(w)))
}

fn boxed(row: &str) -> Option<&str> {
    row.trim_start().strip_prefix(['│', '┃'])
}

/// The text after the input box's prompt (`> `, or `! ` in shell mode)
/// on a row inside the box's side.
fn prompt_text(row: &str) -> Option<&str> {
    let inside = boxed(row)?.trim_start();
    let typed = inside.strip_prefix("> ").or_else(|| inside.strip_prefix("! "))?;
    Some(unboxed(typed))
}

/// gemini is still at a turn: its spinner or "esc to cancel" is at the
/// bottom, or neither its input box nor a list to pick from is.  Only
/// asked of a screen that holds no dialog and no approval.
pub(super) fn is_busy(screen: &str) -> bool {
    let spinner = |r: &str| unboxed(r).chars().next().is_some_and(|c| ('\u{2801}'..='\u{28ff}').contains(&c));
    let rows: Vec<&str> = bottom(screen).collect();
    rows.iter().any(|r| r.contains("esc to cancel") || spinner(r)) || !rows.iter().any(|r| prompt_text(r).is_some())
}

/// Something is typed in gemini's input box and not sent.  The box's
/// next rows inside its sides hold the rest of a long entry.
pub(super) fn has_typed(screen: &str) -> bool {
    let rows: Vec<&str> = bottom(screen).collect();
    let Some(i) = rows.iter().rposition(|r| prompt_text(r).is_some()) else { return false };
    let first = prompt_text(rows[i]).unwrap_or("");
    let more = rows[i + 1..].iter().map_while(|r| boxed(r)).any(|r| !unboxed(r).is_empty());
    (!first.is_empty() && first != PLACEHOLDER) || more
}

#[cfg(test)]
#[path = "screen_tests.rs"]
mod tests;

//! The status line `pane.run` types after a command, kept out of the
//! text handed to a caller.
//!
//! The line editor echoes the line between the command and its output.
//! The window still shows it; a reader of the pane gets the screen
//! without it.  Only the line's own text is cut: what stands before it
//! on its row, a prompt or output that did not end its row, is kept,
//! as nothing on the screen tells the two apart for certain.  A row
//! with nothing left is dropped.  In a pane narrower than the line the
//! editor wraps it over several rows; the text read here has no wrap
//! marks, so a row that holds only the end of the line is joined with
//! the rows above it that hold the rest.  A space that fell at the end
//! of a row may be missing there.

use marspot::shell_proto::run_trailer;

/// What stands before the status line on `line` (one row, or a logical
/// line) that ends with it, without trailing spaces.
pub(crate) fn before_status(line: &str) -> Option<&str> {
    let line = line.trim_end();
    forms().into_iter().find_map(|form| line.strip_suffix(form)).map(str::trim_end)
}

/// `text` (rows joined by newlines) without the status lines on it.
pub(crate) fn drop_status_lines(text: &str) -> String {
    let rows: Vec<&str> = text.split('\n').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(rows.len());
    // the first row below the last status line; every row from here on is kept whole
    let mut below = 0;
    for (at, &row) in rows.iter().enumerate() {
        let cut = before_status(row).map(|before| (0, before)).or_else(|| wrapped(&rows[below..at], row));
        let Some((above, before)) = cut else {
            kept.push(row);
            continue;
        };
        kept.truncate(kept.len() - above);
        if !before.is_empty() {
            kept.push(before);
        }
        below = at + 1;
    }
    kept.join("\n")
}

fn forms() -> [&'static str; 2] {
    [run_trailer(false), run_trailer(true)]
}

/// How many rows at the end of `above` hold the start of a status line
/// that `last` holds only the end of, and what stands before it on the
/// first of them.
fn wrapped<'a>(above: &[&'a str], last: &str) -> Option<(usize, &'a str)> {
    let last = last.trim_end();
    if last.is_empty() {
        return None;
    }
    forms().into_iter().find_map(|form| {
        let mut want = form.strip_suffix(last)?.trim_end();
        for (k, row) in above.iter().rev().enumerate() {
            let row = row.trim_end();
            if let Some(before) = row.strip_suffix(want) {
                return Some((k + 1, before.trim_end()));
            }
            if row.is_empty() {
                return None;
            }
            want = want.strip_suffix(row)?.trim_end();
        }
        None
    })
}

#[cfg(test)]
#[path = "run_trailer_tests.rs"]
mod tests;

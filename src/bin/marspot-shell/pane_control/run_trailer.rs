//! The status line `pane.run` types after a command, kept out of the
//! text handed to a caller.
//!
//! The line editor echoes the line between the command and its output.
//! The window still shows it; a reader of the pane gets the screen
//! without it.  A row is the line when it ends with the line's exact
//! text and what the editor put before it is blank or a short prompt
//! (bash and sh put their `> ` continuation prompt); a row where other
//! text shares the line is kept whole, so no output is lost.  In a pane
//! narrower than the line the editor wraps it over several rows; the
//! text read here has no wrap marks, so rows are joined, spaces ignored,
//! when the last holds only the end of the line and the rows above it
//! hold the rest, the first under the same prompt rule.

use std::sync::LazyLock;

use marspot::shell_proto::run_trailer;

/// The most rows one wrapped status line is looked for across.
const MAX_WRAPPED_ROWS: usize = 8;

/// The most non-space characters before the line that still read as
/// the editor's prompt rather than output.
const MAX_PROMPT_CHARS: usize = 2;

/// Each form of the line, and the same without its spaces.
static FORMS: LazyLock<[(&str, String); 2]> = LazyLock::new(|| {
    let form = |t: &'static str| (t, t.chars().filter(|c| !c.is_whitespace()).collect());
    [form(run_trailer(false)), form(run_trailer(true))]
});

fn short_prompt(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).count() <= MAX_PROMPT_CHARS
}

/// `line` (one row, or a logical line) is the echoed status line.
pub(crate) fn is_status_line(line: &str) -> bool {
    let line = line.trim_end();
    FORMS.iter().any(|(t, _)| line.strip_suffix(t).is_some_and(short_prompt))
}

/// `text` (rows joined by newlines) without the rows that show the
/// status line.
pub(crate) fn drop_status_lines(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for row in text.split('\n') {
        if is_status_line(row) {
            continue;
        }
        match wrapped_above(&kept, row) {
            Some(k) => kept.truncate(kept.len() - k),
            None => kept.push(row),
        }
    }
    kept.join("\n")
}

/// How `row`'s non-space text sits at the end of `want`.
enum Fit<'a> {
    /// all of the row: what of `want` is left before it
    Whole(&'a str),
    /// all of `want`: the non-space text of the row before it
    Rest(usize),
    No,
}

fn fit<'a>(want: &'a str, row: &str) -> Fit<'a> {
    let mut want_end = want.len();
    let mut chars = row.char_indices().rev().filter(|(_, c)| !c.is_whitespace());
    for (_, c) in chars.by_ref() {
        match want[..want_end].chars().next_back() {
            Some(w) if w == c => want_end -= w.len_utf8(),
            Some(_) => return Fit::No,
            None => return Fit::Rest(1 + chars.count()),
        }
    }
    Fit::Whole(&want[..want_end])
}

/// How many rows at the end of `above` hold the start of a status line
/// that `last` ends.
fn wrapped_above(above: &[&str], last: &str) -> Option<usize> {
    let end = last.trim_end().chars().next_back()?;
    FORMS.iter().filter(|(t, _)| t.ends_with(end)).find_map(|(_, whole)| {
        // the last row holds only the end; a whole line on one row was judged above
        let Fit::Whole(mut want) = fit(whole, last) else { return None };
        if want.is_empty() {
            return None;
        }
        for (k, row) in above.iter().rev().take(MAX_WRAPPED_ROWS).enumerate() {
            match fit(want, row) {
                Fit::Whole("") => return Some(k + 1),
                Fit::Whole(rest) => want = rest,
                Fit::Rest(n) if n <= MAX_PROMPT_CHARS => return Some(k + 1),
                Fit::Rest(_) | Fit::No => return None,
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh() -> &'static str {
        run_trailer(false)
    }

    /// `line` cut into rows of `cols` characters, as a narrow pane wraps it.
    fn wrap(line: &str, cols: usize) -> Vec<String> {
        line.chars().collect::<Vec<_>>().chunks(cols).map(|c| c.iter().collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn pane_control_status_line_is_left_out_as_zsh_and_bash_echo_it() {
        let zsh = format!("READY> echo hi\n{}\nhi\nREADY>", sh());
        assert_eq!(drop_status_lines(&zsh), "READY> echo hi\nhi\nREADY>");
        let bash = format!("$ echo hi\n> {}\nhi\n$", sh());
        assert_eq!(drop_status_lines(&bash), "$ echo hi\nhi\n$");
        let fish = format!("> true\n{}\n>", run_trailer(true));
        assert_eq!(drop_status_lines(&fish), "> true\n>");
        assert!(is_status_line(&format!("> {}  ", sh())));
        assert!(is_status_line(&format!("%# {}", sh())));
    }

    #[test]
    fn pane_control_status_line_wrapped_over_rows_is_left_out() {
        for line in [sh().to_string(), format!("> {}", sh())] {
            for cols in [10, 17, 30, 40] {
                let rows = wrap(&line, cols);
                let text = format!("$ echo hi\n{}\nhi\n$", rows.join("\n"));
                if rows.len() <= MAX_WRAPPED_ROWS + 1 {
                    assert_eq!(drop_status_lines(&text), "$ echo hi\nhi\n$", "{cols} columns: {text}");
                }
            }
        }
    }

    #[test]
    fn pane_control_output_sharing_the_row_with_the_status_line_is_kept() {
        let row = format!("hi there{}", sh());
        assert!(!is_status_line(&row));
        let text = format!("$ echo hi\n{row}\n$");
        assert_eq!(drop_status_lines(&text), text);
        let row = format!("abc {}", sh());
        assert!(!is_status_line(&row));
    }

    #[test]
    fn pane_control_output_sharing_the_first_wrapped_row_is_kept() {
        let rows = wrap(&format!("output {}", sh()), 20);
        let text = format!("$ echo hi\n{}\n$", rows.join("\n"));
        assert_eq!(drop_status_lines(&text), text);
        let rows = wrap(&format!("%  {}", sh()), 20);
        let text = format!("$ echo hi\n{}\n$", rows.join("\n"));
        assert_eq!(drop_status_lines(&text), "$ echo hi\n$");
    }

    #[test]
    fn pane_control_lines_that_only_mention_the_mark_are_kept() {
        let text = "echo '133;D'\n133;D\nprintf '\\033]133;D;%s\\007' $?\nmarspot=run\\007' $? and more\n$?";
        assert_eq!(drop_status_lines(text), text);
        assert!(!is_status_line(&format!("{} # note", sh())));
        assert_eq!(drop_status_lines(""), "");
        let text = "status\nis\nyes";
        assert_eq!(drop_status_lines(text), text);
        let unspaced: String = sh().split_whitespace().collect();
        let text = format!("$\n{unspaced}");
        assert_eq!(drop_status_lines(&text), text);
    }
}

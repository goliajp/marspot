//! The status line is cut in every form a shell echoes it, and nothing
//! else is.

use super::*;

fn sh() -> &'static str {
    run_trailer(false)
}

/// `line` cut into rows of `cols` characters, as a narrow pane wraps it.
fn wrap(line: &str, cols: usize) -> Vec<String> {
    line.chars().collect::<Vec<_>>().chunks(cols).map(|c| c.iter().collect::<String>().trim_end().to_string()).collect()
}

/// The rows `before` takes up in a pane of `cols`, each ended.
fn left(before: &str, cols: usize) -> String {
    wrap(before.trim_end(), cols).iter().map(|r| format!("{r}\n")).collect()
}

#[test]
fn pane_control_status_line_is_left_out_as_each_shell_echoes_it() {
    let zsh = format!("READY> echo hi\n{}\nhi\nREADY>", sh());
    assert_eq!(drop_status_lines(&zsh), "READY> echo hi\nhi\nREADY>");
    let fish = format!("> true\n{}\n>", run_trailer(true));
    assert_eq!(drop_status_lines(&fish), "> true\n>");
    let bash = format!("$ echo hi\n> {}\nhi\n$", sh());
    assert_eq!(drop_status_lines(&bash), "$ echo hi\n>\nhi\n$");
    // bash 3.2 runs the two lines one after the other
    let bash = format!("bash-3.2$ echo hi\nhi\nbash-3.2$ {}\nbash-3.2$", sh());
    assert_eq!(drop_status_lines(&bash), "bash-3.2$ echo hi\nhi\nbash-3.2$\nbash-3.2$");
}

#[test]
fn pane_control_what_shares_the_row_with_the_status_line_is_kept() {
    for before in ["hi there", "abc ", "foobash-3.2$ ", "4$ ", "{}$ ", "}$ ", "hello", "100%", "x: ", "✓ ❯ "] {
        for below in ["", "\n", "\n$", "\nnew prompt>"] {
            let text = format!("$ out\n{before}{}{below}", sh());
            assert_eq!(drop_status_lines(&text), format!("$ out\n{}{below}", before.trim_end()));
        }
    }
}

#[test]
fn pane_control_status_line_wrapped_over_rows_is_cut_the_same() {
    for before in ["", "> ", "%  ", "bash-3.2$ ", "output ", "foobash-3.2$ ", "}$ "] {
        for form in forms() {
            for cols in 4..80 {
                let rows = wrap(&format!("{before}{form}"), cols).join("\n");
                let text = format!("$ echo hi\n{rows}\nhi\n$");
                let want = format!("$ echo hi\n{}hi\n$", left(before, cols));
                assert_eq!(drop_status_lines(&text), want, "{cols} columns: {text}");
            }
        }
    }
}

#[test]
fn pane_control_rows_that_only_look_like_part_of_the_status_line_are_kept() {
    let (start, end) = sh().split_at(sh().len() - 2);
    // another row, or a blank one, stands between the two
    for between in ["hi", ""] {
        let text = format!("{start}\n{between}\n{end}");
        assert_eq!(drop_status_lines(&text), text);
    }
    // each row belongs to one line only
    let text = format!("$ {start}\n{end}\n{end}\n$");
    assert_eq!(drop_status_lines(&text), format!("$\n{end}\n$"));
    let text = format!("{start}\n{}\n{end}", sh());
    assert_eq!(drop_status_lines(&text), format!("{start}\n{end}"));
}

#[test]
fn pane_control_lines_that_only_mention_the_mark_are_kept() {
    let text = "echo '133;D'\n133;D\nprintf '\\033]133;D;%s\\007' $?\nmarspot=run\\007' $? and more\n$?";
    assert_eq!(drop_status_lines(text), text);
    let noted = format!("{} # note", sh());
    assert_eq!(drop_status_lines(&noted), noted);
    assert_eq!(drop_status_lines(""), "");
    let text = "status\nis\nyes\n\n  indented  \n";
    assert_eq!(drop_status_lines(text), text);
    let unspaced: String = sh().split_whitespace().collect();
    for text in [format!("$\n{unspaced}"), format!("$\n{}", wrap(&unspaced, 20).join("\n"))] {
        assert_eq!(drop_status_lines(&text), text);
    }
}

#[test]
fn pane_control_every_status_line_of_many_runs_is_left_out() {
    let run = format!("% true\n{}\n", sh());
    let text = format!("notice\n{}", run.repeat(500));
    assert_eq!(drop_status_lines(&text), format!("notice\n{}", "% true\n".repeat(500)));
}

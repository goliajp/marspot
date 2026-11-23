//! `matched_line` never shows the status line a run typed.

use super::*;
use crate::pane_control::matched::Replay;

fn status_line() -> &'static str {
    marspot::shell_proto::run_trailer(false)
}

/// The line answered for `needle` at row `line` of a pane showing
/// `rows`, and where it is from.
fn matched(rows: &[&str], line: u64, needle: &str) -> (Option<String>, Option<String>) {
    let mut term = Terminal::new(100, 10);
    term.feed(rows.join("\r\n").as_bytes());
    let replay = Replay { live_scroll: term.grid().scroll_push_count(), term };
    let mut v = obj([("text", run_trailer::drop_status_lines(&rows.join("\n")).into())]);
    matched_line(&mut v, Some(&replay), line, needle);
    let at = |key| v.str_at(key).map(String::from);
    (at("matched_line"), at("matched_line_from"))
}

#[test]
fn pane_control_matched_line_on_the_status_line_itself_is_not_said() {
    for prompt in ["", "> ", "bash-3.2$ "] {
        let typed = format!("{prompt}{}", status_line());
        let rows = ["bash-3.2$ echo hi", "hi", typed.as_str(), "bash-3.2$"];
        assert_eq!(matched(&rows, 2, "marspot=run"), (None, None), "{prompt:?}");
    }
    let typed = format!("bash-3.2$ {}", status_line());
    let rows = ["bash-3.2$ echo printf", "printf", typed.as_str(), "bash-3.2$"];
    assert_eq!(matched(&rows, 2, "printf"), (Some("printf".into()), Some("screen".into())));
}

#[test]
fn pane_control_matched_line_is_the_text_before_the_status_line() {
    let typed = format!("foobash-3.2$ {}", status_line());
    let rows = ["bash-3.2$ printf foo", typed.as_str(), "bash-3.2$"];
    assert_eq!(matched(&rows, 1, "foo"), (Some("foobash-3.2$".into()), Some("row".into())));
    // the row is no longer in the record: the screen's line, cut the same
    assert_eq!(matched(&rows, 900, "foobash"), (Some("foobash-3.2$".into()), Some("screen".into())));
    assert_eq!(matched(&rows, 1, "marspot=run"), (None, None));
    assert_eq!(matched(&rows, 0, "printf foo"), (Some("bash-3.2$ printf foo".into()), Some("row".into())));
}

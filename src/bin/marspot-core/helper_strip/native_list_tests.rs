//! Where a list's figures end, for rows whose tasks say words the
//! figures use, and for rows cut at a narrow pane's edge.

use super::native_list::find;
use marspot_term::terminal::Terminal;

/// The columns of a list of `rows` under `main`, drawn `cols` wide.
fn columns(cols: u16, rows: &[&str]) -> Option<(u16, u16)> {
    let rule = "─".repeat(cols as usize);
    let mut t = Terminal::new(cols, 12);
    t.feed(format!("{rule}\r\n❯\r\n{rule}\r\n  foot\r\n\r\n  ⏺ main\r\n{}", rows.join("\r\n")).as_bytes());
    find(t.grid()).unwrap().columns.map(|c| (c.names, c.right))
}

#[test]
fn figures_in_a_task_are_not_the_rows_figures() {
    for task in ["Fix failed tests", "Wait 30s for a signal", "stopped 2m ago"] {
        let row = format!("  ◯ general-purpose  waiting on 1 shell · {task} · since 5:23 PM");
        assert_eq!(columns(136, &[&row]), Some((15, 134)), "{task}");
    }
}

#[test]
fn a_row_with_figures_sets_the_edge_over_a_waiting_one() {
    let waiting = "  ◯ general-purpose  waiting on 1 shell · Fix failed tests · since 5:23 PM";
    let figures = "failed · 21s · ↓ 11.9k tokens";
    // figures that end at 130, short of where they would on their own
    let pad = " ".repeat(130 - 21 - "short".len() - figures.chars().count());
    let running = format!("  ◯ tiny             short{pad}{figures}");
    assert_eq!(columns(136, &[waiting, &running]), Some((15, 130)));
    assert_eq!(columns(136, &[&running, waiting]), Some((15, 130)));
}

#[test]
fn figures_cut_too_short_to_read_end_at_the_edge() {
    let name = "an-agent-type-whose-name-is…";
    assert_eq!(columns(35, &[&format!("  ◯ {name} 1…"), &format!("  ◯ {name} …")]), Some((28, 35)));
    assert_eq!(columns(40, &[&format!("  ◯ {name} 17s · …")]), Some((28, 40)));
}

#[test]
fn a_task_cut_short_in_its_column_is_not_figures() {
    let row = "  ◯ general-purpose  waiting on 1 shell · Wait 30s · s…";
    assert_eq!(columns(136, &[row]), Some((15, 134)));
}

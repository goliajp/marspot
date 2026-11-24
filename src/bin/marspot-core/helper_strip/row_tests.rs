//! What a composed row says, and which rows there are.

use super::compose::{compose, Arrow, List, Pick, Row, State};
use super::compose_tests::strip_of;
use super::figures::{compact, cut, elapsed};
use super::native_list::Columns;
use super::strip::Strip as One;
use super::tests::text;
use super::theme::THEMES;

#[test]
fn times_are_written_as_claude_writes_them() {
    let m = 60_000;
    let h = 60 * m;
    for (ms, want) in [
        (0, "0s"),
        (999, "0s"),
        (59_999, "59s"),
        (m, "1m 0s"),
        (m + 499, "1m 0s"),
        (m + 500, "1m 1s"),
        (2 * m - 500, "2m 0s"),
        (46 * m + 54_000, "46m 54s"),
        (h - 1, "1h 0m 0s"),
        (h + 2 * m + 3_000, "1h 2m 3s"),
        (24 * h - 1, "1d 0h 0m"),
        (49 * h + 5 * m, "2d 1h 5m"),
    ] {
        assert_eq!(elapsed(ms).as_str(), want, "{ms}");
    }
}

#[test]
fn tokens_are_written_as_claude_writes_them() {
    for (n, want) in [
        (7, "7"),
        (999, "999"),
        (1000, "1.0k"),
        (12_049, "12.0k"),
        (12_050, "12.1k"),
        (115_300, "115.3k"),
        (999_949, "999.9k"),
        (999_950, "1.0m"),
        (1_234_567, "1.2m"),
        (2_000_000_000, "2.0b"),
        (u64::MAX, "18446744.1t"),
    ] {
        assert_eq!(compact(n).as_str(), want, "{n}");
    }
}

#[test]
fn text_is_cut_between_whole_characters() {
    assert_eq!(cut("abc", 3), ("abc", ""));
    assert_eq!(cut("abcd", 3), ("ab", "…"));
    assert_eq!(cut("正在核对", 8), ("正在核对", ""));
    assert_eq!(cut("正在核对每", 8), ("正在核", "…"));
    assert_eq!(cut("正在", 1), ("", "…"));
    assert_eq!(cut("正在", 0), ("", ""));
    assert_eq!(cut("か\u{3099}き\u{3099}x", 4), ("か\u{3099}", "…"));
}

const ROW: Row = Row {
    name: "codex",
    named: false,
    task: "job",
    state: State::Running,
    elapsed_ms: 3_000,
    tokens: None,
    arrow: Arrow::Up,
    gone: false,
};

fn lines(rows: &[Row], selected: Option<Pick>, viewed: Pick, height: u16) -> Vec<String> {
    let l = List { rows, selected, viewed, theme: &THEMES[0], native: None };
    let strip = strip_of(50, height, &l);
    (0..height).map(|r| text(strip.grid(), r).trim_end().to_string()).collect()
}

#[test]
fn figures_say_what_is_known() {
    let at = |row: Row| lines(&[row], None, Pick::Main, 3)[2].split("job").nth(1).unwrap().trim().to_string();
    assert_eq!(at(ROW), "3s", "no tokens: nothing in their place");
    assert_eq!(at(Row { tokens: Some(0), ..ROW }), "3s");
    assert_eq!(at(Row { tokens: Some(12), ..ROW }), "3s · ↑ 12 tokens", "no tool called yet");
    assert_eq!(at(Row { tokens: Some(1200), arrow: Arrow::Down, ..ROW }), "3s · ↓ 1.2k tokens");
    assert_eq!(at(Row { state: State::Failed, ..ROW }), "failed · 3s");
    assert_eq!(at(Row { state: State::Stopped, tokens: Some(9), ..ROW }), "stopped · 3s · ↑ 9 tokens");
    assert_eq!(at(Row { state: State::Awaiting, tokens: Some(9), ..ROW }), "awaiting approval");
}

/// How a row ended takes room from its own task only.
#[test]
fn an_ended_rows_figures_leave_the_other_rows_as_they_were() {
    let long = Row { task: "a task that is far too long for the room there is in this pane", ..ROW };
    let end = |l: &str| l.find('…').unwrap();
    let before = lines(&[long, long], None, Pick::Main, 4);
    let after = lines(&[long, Row { state: State::Stopped, ..long }], None, Pick::Main, 4);
    assert_eq!(before[2], after[2]);
    assert_eq!(end(&before[3]) - end(&after[3]), "stopped · ".chars().count());
}

#[test]
fn five_rows_show_and_follow_the_selected_one() {
    let names: Vec<String> = (0..7).map(|i| format!("n{i}")).collect();
    let rows: Vec<Row> = names.iter().map(|n| Row { name: n, ..ROW }).collect();
    let of = |selected, viewed| lines(&rows, selected, viewed, 8);
    let top = of(None, Pick::Main);
    assert!(top[2].contains("n0") && top[6].contains("n4"));
    assert_eq!((top[1].as_str(), top[7].as_str()), ("  ⏺ main", "  ↓ 2 more"));
    let mid = of(Some(Pick::Row(5)), Pick::Main);
    assert!(mid[2].contains("n1") && mid[6].starts_with("❯ ◯ n5"));
    assert!(mid[1].ends_with("↑ 1 more") && mid[1].chars().count() == 48, "{:?}", mid[1]);
    assert_eq!(mid[7], "  ↓ 1 more");
    let end = of(Some(Pick::Row(6)), Pick::Main);
    assert_eq!(end[7], "", "its row stays, blank");
    let viewed = of(Some(Pick::Main), Pick::Row(6));
    assert!(viewed[1].starts_with("❯ ◯ main") && viewed[6].starts_with("  ⏺ n6"));
}

#[test]
fn a_row_that_is_gone_shows_while_it_is_in_view() {
    let rows = [ROW, Row { name: "left", gone: true, ..ROW }];
    assert_eq!(lines(&rows, None, Pick::Main, 4)[3], "");
    assert!(lines(&rows, None, Pick::Row(1), 4)[3].starts_with("  ⏺ left"));
}

#[test]
fn a_list_that_shrinks_leaves_nothing_behind() {
    let mut strip = One::new(4);
    strip.fit(50, 4);
    let l = |rows| List { rows, selected: None, viewed: Pick::Main, theme: &THEMES[0], native: None };
    compose(&mut strip, &l(&[ROW, ROW]));
    let seq = strip.seq();
    compose(&mut strip, &l(&[ROW, ROW]));
    assert_eq!(strip.seq(), seq, "the same list again writes nothing");
    compose(&mut strip, &l(&[ROW]));
    assert_eq!(text(strip.grid(), 3).trim(), "");
}

/// A pane too narrow for anything is drawn without a fault.
#[test]
fn no_width_is_too_narrow() {
    for cols in 1..60 {
        let rows = [ROW, Row { name: "正在核对每一项的结果", task: "正", state: State::Failed, ..ROW }];
        for native in [None, Some(Columns { names: 28, right: cols })] {
            let l = List { rows: &rows, selected: Some(Pick::Row(1)), viewed: Pick::Main, theme: &THEMES[0], native };
            strip_of(cols, 5, &l);
        }
    }
}

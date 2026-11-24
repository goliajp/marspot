//! Composed rows against Claude Code's own, as it drew them.
//!
//! `native.txt` holds the bottom of real screens: `@` a screen's name
//! and size, `>` a row as data, `=` what is selected and what is in
//! view, then every run of cells from the input box down.

use super::compose::{compose, Arrow, List, Pick, Row, State};
use super::native_list::{find, NativeList};
use super::strip::Strip as One;
use super::tests::text;
use super::theme::{Theme, THEMES};
use marspot_term::grid::{Cell, CellAttrs, Grid};
use marspot_term::terminal::Terminal;

struct Screen {
    name: &'static str,
    term: Terminal,
    rows: Vec<Row<'static>>,
    selected: Option<Pick>,
    viewed: Pick,
}

fn sgr(fg: &str, background: bool) -> String {
    let base = if background { 40 } else { 30 };
    let names = ["black", "red", "green", "brown", "blue", "magenta", "cyan", "white"];
    if fg == "default" {
        return format!("{}", base + 9);
    }
    if let Some(i) = names.iter().position(|n| *n == fg) {
        return format!("{}", base + i);
    }
    if let Some(i) = fg.strip_prefix("bright").and_then(|f| names.iter().position(|n| *n == f)) {
        return format!("{}", base + 60 + i);
    }
    if let Some(i) = fg.strip_prefix('i') {
        return format!("{};5;{i}", base + 8);
    }
    let part = |i: usize| u8::from_str_radix(&fg[i..i + 2], 16).expect("a colour");
    format!("{};2;{};{};{}", base + 8, part(0), part(2), part(4))
}

fn pick(s: &str) -> Option<Pick> {
    match s {
        "-" => None,
        "main" => Some(Pick::Main),
        n => Some(Pick::Row(n.parse().unwrap())),
    }
}

fn row(line: &'static str) -> Row<'static> {
    let f: Vec<&str> = line.split('|').collect();
    Row {
        name: f[0],
        named: f[1] == "named",
        task: f[2],
        state: match f[3] {
            "run" => State::Running,
            "done" => State::Done,
            "failed" => State::Failed,
            "stopped" => State::Stopped,
            s => panic!("{s}"),
        },
        elapsed_ms: f[4].parse().unwrap(),
        tokens: Some(f[5].parse().unwrap()),
        arrow: Arrow::Down,
    }
}

fn draw(term: &mut Terminal, line: &str) {
    let f: Vec<&str> = line.splitn(6, ' ').collect();
    let cols = term.grid().cols() as usize;
    let body = if f[5] == "─*" { "─".repeat(cols) } else { f[5].to_string() };
    let flag = |c: char, n: &str| if f[4].contains(c) { n.to_string() } else { String::new() };
    let at: Vec<u16> = f[..2].iter().map(|n| n.parse::<u16>().unwrap() + 1).collect();
    let out = format!(
        "\x1b[{};{}H\x1b[0{}{};{};{}m{body}\x1b[0m",
        at[0], at[1], flag('B', ";1"), flag('D', ";2"), sgr(f[2], false), sgr(f[3], true)
    );
    term.feed(out.as_bytes());
}

fn screens() -> Vec<Screen> {
    let mut out: Vec<Screen> = Vec::new();
    for line in include_str!("native.txt").lines() {
        let (kind, rest) = line.split_at(2);
        match kind {
            "@ " => {
                let f: Vec<&str> = rest.split(' ').collect();
                let term = Terminal::new(f[1].parse().unwrap(), f[2].parse().unwrap());
                out.push(Screen { name: f[0], term, rows: Vec::new(), selected: None, viewed: Pick::Main });
            }
            "> " => out.last_mut().unwrap().rows.push(row(rest)),
            "= " => {
                let s = out.last_mut().unwrap();
                let (selected, viewed) = rest.split_once(' ').unwrap();
                s.selected = pick(selected);
                s.viewed = pick(viewed).unwrap_or(Pick::Main);
            }
            _ => draw(&mut out.last_mut().unwrap().term, line),
        }
    }
    // the cursor is on the input line, off the list
    for s in &mut out {
        s.term.feed(b"\x1b[H");
    }
    out
}

fn theme(s: &Screen) -> &'static Theme {
    Theme::of_screen(s.term.grid()).unwrap_or_else(|| panic!("{}: no theme", s.name))
}

fn list<'a>(s: &'a Screen, theme: &'a Theme, native: bool) -> List<'a> {
    let columns = find(s.term.grid()).and_then(|n| n.columns);
    List {
        rows: &s.rows,
        selected: s.selected,
        viewed: s.viewed,
        theme,
        native: if native { columns } else { None },
    }
}

pub(super) fn strip_of(cols: u16, rows: u16, list: &List) -> One {
    let mut strip = One::new(rows);
    strip.fit(cols, rows);
    compose(&mut strip, list);
    strip
}

/// A blank's ink is seen nowhere; everything else of a cell is.
fn seen(c: Cell) -> Cell {
    let blank = CellAttrs { bg: c.attrs.bg, reverse: c.attrs.reverse, underline: c.attrs.underline, ..Default::default() };
    if c.ch == ' ' { Cell { ch: ' ', attrs: blank } } else { c }
}

fn same_rows(what: &str, mine: &Grid, from: u16, native: &Grid, top: u16, rows: u16) {
    for r in 0..rows {
        for c in 0..native.cols() {
            let (a, b) = (seen(mine.cell(c, from + r)), seen(native.cell(c, top + r)));
            assert_eq!(a, b, "{what} row {r} col {c}\n{}\n{}", text(mine, from + r), text(native, top + r));
        }
    }
}

#[test]
fn rows_are_claudes_own_cell_for_cell() {
    let all = screens();
    for width in [120, 100, 80, 60, 49, 40] {
        assert!(all.iter().any(|s| s.term.grid().cols() == width), "none {width} wide");
    }
    for s in &all {
        let g = s.term.grid();
        let n = find(g).unwrap_or_else(|| panic!("{}: no list", s.name));
        let rows = n.last_row - n.main_row + 1;
        // a line more: claude's own is cut short at a screen's end
        let strip = strip_of(g.cols(), rows + 2, &list(s, theme(s), false));
        same_rows(s.name, strip.grid(), 1, g, n.main_row, rows);
    }
}

#[test]
fn rows_that_go_on_from_claudes_own_have_its_columns() {
    for s in &screens() {
        let g = s.term.grid();
        let n = find(g).unwrap();
        let rows = n.last_row - n.main_row;
        let strip = strip_of(g.cols(), rows + 1, &list(s, theme(s), true));
        same_rows(s.name, strip.grid(), 0, g, n.main_row + 1, rows);
        assert!(s.rows.len() > 5 || text(strip.grid(), rows).trim().is_empty(), "{}", s.name);
    }
}

#[test]
fn a_wider_native_list_sets_the_columns() {
    let all = screens();
    let s = all.iter().find(|s| s.name == "c-scroll").unwrap();
    let only = [Row { name: "a-name-longer-than-twenty-eight", task: "t", ..s.rows[1] }];
    let l = List { rows: &only, selected: None, viewed: Pick::Main, ..list(s, theme(s), true) };
    let strip = strip_of(100, 2, &l);
    let row = text(strip.grid(), 0);
    assert!(row.starts_with("  ◯ a-name-longer-than-twenty-e…  t "), "{row:?}");
    assert_eq!(strip.grid().cell(97, 0).ch, 's');
}

#[test]
fn the_list_is_found_with_what_is_selected() {
    let all = screens();
    let of = |name: &str| -> NativeList {
        let s = all.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("{name}"));
        find(s.term.grid()).unwrap()
    };
    let names = |l: &NativeList| l.columns.unwrap().names;
    let b = of("b-1");
    assert_eq!((b.main_row, b.rows, b.last_row, b.selected, b.on_last), (36, 3, 39, None, false));
    assert_eq!((names(&b), b.columns.unwrap().right), (14, 98));
    assert_eq!((of("b-down1").selected, of("b-down1").on_last), (Some(Pick::Main), false));
    assert_eq!((of("b-down2").selected, of("b-down2").on_last), (Some(Pick::Row(0)), false));
    assert_eq!((of("b-down5").selected, of("b-down5").on_last), (Some(Pick::Row(2)), true));
    let more = of("c-1");
    assert_eq!((more.rows, more.last_row, more.main_row), (5, 39, 33), "the row that says how many more");
    let scrolled = of("c-scroll");
    assert_eq!((scrolled.last_row, scrolled.selected, scrolled.on_last), (38, Some(Pick::Row(4)), true));
    assert_eq!(names(&scrolled), 28, "from where the tasks are, not from the names in view");
    assert_eq!(names(&of("sweep7-40x24")), 14, "with no room for tasks");
    assert_eq!(names(&of("sweep29-37x24")), 10);
    assert_eq!(of("i-list").last_row, 30, "blank rows may follow");
}

#[test]
fn what_only_looks_like_a_list_is_none() {
    let none = |bytes: &str| {
        let mut t = Terminal::new(80, 12);
        t.feed(bytes.as_bytes());
        find(t.grid())
    };
    let rule = "─".repeat(80);
    let good = format!("{rule}\r\n❯\r\n{rule}\r\n  foot\r\n\r\n  ⏺ main\r\n  ◯ tiny  job   1s");
    assert!(none(&good).is_some());
    assert_eq!(none(""), None);
    assert_eq!(none(&good.replace('─', "-")), None, "no input box");
    assert_eq!(none(&good.replace("foot\r\n", "foot")), None, "main right under the footer");
    assert_eq!(none(&good.replace("main", "mainly")), None);
    assert_eq!(none(&good.replace("  ⏺ main", "⏺ main")), None, "a message");
    assert_eq!(none(&format!("{good}\r\nmore text")), None, "it is the last thing on a screen");
    let alone = none(&good.replace("\r\n  ◯ tiny  job   1s", "")).unwrap();
    assert_eq!((alone.rows, alone.columns, alone.last_row), (0, None, 5));
}

#[test]
fn the_theme_is_the_one_the_agent_draws_in() {
    let all = screens();
    for (name, i) in [
        ("b-1", 0),
        ("t-light-0", 1),
        ("t-light-2", 1),
        ("t-dark-daltonized-0", 2),
        ("t-dark-daltonized-2", 2),
        ("t-dark-ansi-0", 4),
        ("t-light-ansi-2", 5),
        ("t-256-0", 6),
    ] {
        let s = all.iter().find(|s| s.name == name).unwrap();
        assert!(std::ptr::eq(theme(s), &THEMES[i]), "{name}");
    }
    let mut t = Terminal::new(40, 5);
    t.feed("──────\r\n$ ls".as_bytes());
    assert!(Theme::of_screen(t.grid()).is_none());
    assert_eq!(Theme::for_paper(false), &THEMES[0]);
    assert_eq!(Theme::for_paper(true), &THEMES[1]);
}

#[test]
fn a_row_with_no_figures_leaves_them_at_the_right_edge() {
    let rule = "─".repeat(136);
    let waiting = "  ◯ general-purpose  waiting on 1 shell · Wait for a signal · since 5:23 PM";
    let mut t = Terminal::new(136, 12);
    t.feed(format!("{rule}\r\n❯\r\n{rule}\r\n  foot\r\n\r\n  ⏺ main\r\n{waiting}").as_bytes());
    let n = find(t.grid()).unwrap();
    assert_eq!(n.columns.map(|c| (c.names, c.right)), Some((15, 134)));
    let mine = [Row {
        name: "codex",
        named: false,
        task: "review the parser",
        state: State::Running,
        elapsed_ms: 12_000,
        tokens: None,
        arrow: Arrow::Down,
    }];
    let l = List { rows: &mine, selected: None, viewed: Pick::Main, theme: &THEMES[0], native: n.columns };
    let strip = strip_of(136, 1, &l);
    let row = text(strip.grid(), 0);
    assert!(row.starts_with("  ◯ codex            review the parser "), "{row:?}");
    assert_eq!(row.trim_end().chars().count(), 134, "{row:?}");
}

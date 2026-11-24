//! Composing a list again every second, and finding the agent's own on
//! its screen, allocate nothing.

use super::compose::{compose, Arrow, List, Pick, Row, State};
use super::native_list::find;
use super::strip::Strip;
use super::theme::{Theme, THEMES};
use marspot_term::terminal::Terminal;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// What this thread has allocated since it began to be counted.
    static ALLOCS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct Counting;

fn note() {
    let _ = ALLOCS.try_with(|a| a.set(a.get().map(|n| n + 1)));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

pub(super) fn counted<T>(f: impl FnOnce() -> T) -> usize {
    ALLOCS.with(|a| a.set(Some(0)));
    let out = f();
    let n = ALLOCS.with(|a| a.take()).unwrap();
    drop(out);
    n
}

#[test]
fn the_meter_sees_an_allocation() {
    assert!(counted(|| vec![1u8; 64]) >= 1);
}

#[test]
fn a_list_composed_again_allocates_nothing() {
    let row = Row {
        name: "か\u{3099}き\u{3099}",
        named: true,
        task: "👩\u{200d}👩\u{200d}👧 正在核对每一项的结果然后汇报给主会话",
        state: State::Running,
        elapsed_ms: 0,
        tokens: Some(999_000),
        arrow: Arrow::Down,
    };
    let mut rows = [row; 7];
    let mut strip = Strip::new(8);
    strip.fit(60, 8);
    for second in 0..130u64 {
        for (i, r) in rows.iter_mut().enumerate() {
            r.elapsed_ms = second * 1000 + i as u64 * 100;
            r.tokens = Some(999_000 + second * 100);
        }
        let selected = Some(Pick::Row(second as usize % 7));
        let l = List { rows: &rows, selected, viewed: Pick::Main, theme: &THEMES[0], native: None };
        let n = counted(|| compose(&mut strip, &l));
        assert!(second < 2 || n == 0, "second {second}: {n}");
    }
}

#[test]
fn the_agents_list_and_theme_are_found_without_allocating() {
    let mut t = Terminal::new(80, 24);
    let rule = "─".repeat(80);
    t.feed(format!("{rule}\r\n❯\r\n{rule}\r\n  \x1b[38;2;153;153;153mfoot\r\n\r\n  ⏺ main\r\n  ◯ tiny  job   1s").as_bytes());
    let g = t.grid();
    assert!(find(g).is_some() && Theme::of_screen(g).is_some());
    assert_eq!(counted(|| (find(g), Theme::of_screen(g))), 0);
}

//! A warm link scan allocates nothing.
//!
//! The render loop scans every pane every frame, and the project's
//! rule is that per-frame paths allocate nothing.  A global allocator
//! in this test binary counts this thread's allocations inside a
//! measured region; the region is the second scan of a screen, after a
//! first one has sized the buffers.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::{EXISTS, Listed, Screen, corpus, render};
use marspot_linkify::{
    LinkKind, LinkScratch, PathOracle, PathVerdict, ScanOpts, scan_visible_links_into,
    scan_visible_links_with,
};

// Per-thread: the harness runs tests concurrently in one process.
// `const` init, because a lazily initialised thread-local would
// allocate from inside the allocator.
thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    let _ = COUNTING.try_with(|c| {
        if c.get() {
            let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
        }
    });
}

struct Counting;

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

fn allocations<T>(f: impl FnOnce() -> T) -> (usize, T) {
    ALLOCS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (ALLOCS.with(|a| a.get()), out)
}

/// Every path exists, except one carrying an `@`: with a working
/// directory set, `ops@golia.jp` would otherwise resolve as a relative
/// file before the address is looked at.  Answers without allocating.
struct Always;

impl PathOracle for Always {
    fn probe(&self, path: &str) -> PathVerdict {
        if path.contains('@') { PathVerdict::Missing } else { PathVerdict::Exists }
    }
}

/// A meter that cannot register would read as zero allocations, which
/// is the answer the real test wants — so it has to be shown to count.
#[test]
fn the_meter_counts() {
    let (n, v) = allocations(|| {
        let mut v: Vec<u64> = Vec::with_capacity(4);
        v.extend([1, 2, 3, 4, 5]);
        v
    });
    assert_eq!(v.len(), 5);
    assert!(n >= 2, "an allocation and a grow went unseen: {n}");

    let screen = Screen::new(&["see https://example.com/x"], 40);
    let (n, links) = allocations(|| scan_visible_links_with(&screen, ScanOpts::default(), &Always));
    assert_eq!(links.len(), 1);
    assert!(n > 0, "a cold scan allocates, and the meter must see it");
}

fn mixed_screen() -> Screen {
    let kinds = [
        "url https://example.com/a/b?q=1#frag, then more prose here",
        "mail ops@golia.jp and ask",
        "ipv4 192.168.1.10:8080/status ok",
        "ipv6 [::1]:3000/x and 2001:db8::1",
        "uuid 3acb2d64-504d-49d5-9030-a179087f4bce in the log",
        "abs /usr/local/bin/marspot was built",
        "home ~/Downloads/決算明細_2025-2026.xlsx。",
        "rel src/main.rs:120:5 and ./README.md",
        "file file:///tmp/site/index-ja.html#principles",
        "quoted ~/Library/Safari/\"Favicon Cache\" there",
    ];
    let rows: Vec<&str> = (0..30).map(|r| kinds[r % kinds.len()]).collect();
    Screen::new(&rows, 100)
}

#[test]
fn a_warm_scan_allocates_nothing() {
    let screen = mixed_screen();
    let opts = ScanOpts { cwd: Some("/w/p"), tui_mode: false };
    let mut scratch = LinkScratch::default();
    let mut out = Vec::new();
    let (cold, ()) = allocations(|| scan_visible_links_into(&screen, opts, &Always, &mut scratch, &mut out));
    let first = render(&out);
    assert!(out.len() >= 30, "expected ~30 links, got {}", out.len());
    for kind in [LinkKind::Url, LinkKind::File, LinkKind::Email, LinkKind::Ip, LinkKind::Uuid] {
        assert!(out.iter().any(|l| l.kind == kind), "no {kind:?} in {first}");
    }
    assert!(out.iter().any(|l| l.target.is_some()), "no relative path in {first}");
    assert!(cold > 0, "the first scan sizes the buffers, so it allocates");

    let (warm, ()) = allocations(|| scan_visible_links_into(&screen, opts, &Always, &mut scratch, &mut out));
    assert_eq!(warm, 0, "the second scan allocated {warm} times");
    assert_eq!(render(&out), first);
}

/// The same for every screen in the shared corpus — table cells,
/// composers, wraps — answering from a list rather than a constant.
#[test]
fn every_corpus_screen_rescans_without_allocating() {
    for c in corpus() {
        let mut scratch = LinkScratch::default();
        let mut out = Vec::new();
        scan_visible_links_into(&c.screen, c.opts, &Listed(EXISTS), &mut scratch, &mut out);
        let first = render(&out);
        let (warm, ()) =
            allocations(|| scan_visible_links_into(&c.screen, c.opts, &Listed(EXISTS), &mut scratch, &mut out));
        assert_eq!(warm, 0, "case {:?}: the second scan allocated {warm} times", c.name);
        assert_eq!(render(&out), first, "case {:?}", c.name);
    }
}

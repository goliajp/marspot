//! `scan_visible_links_into` must answer exactly what the allocating
//! scan answers, whatever the buffers it is handed were last used for.
//!
//! `fixtures/scan_corpus.txt` was produced by the scanner as it stood
//! before the buffer-reusing path existed, so the first test compares
//! against that scanner and not just against itself.

mod common;

use common::{EXISTS, Listed, Screen, corpus, render, render_corpus};
use marspot_linkify::{
    LinkKind, LinkRange, LinkScratch, ScanOpts, scan_visible_links_into, scan_visible_links_with,
};

const GOLDEN: &str = include_str!("fixtures/scan_corpus.txt");

#[test]
fn the_corpus_scans_as_it_did_before() {
    let with = render_corpus(|c| scan_visible_links_with(&c.screen, c.opts, &Listed(EXISTS)));
    assert_eq!(with, GOLDEN);
    let into = render_corpus(|c| {
        let mut out = Vec::new();
        scan_visible_links_into(&c.screen, c.opts, &Listed(EXISTS), &mut LinkScratch::default(), &mut out);
        out
    });
    assert_eq!(into, GOLDEN);
}

/// One scratch and one output, reused across every screen in several
/// orders: large after small, small after large, and the same screen
/// twice in a row.
#[test]
fn reused_buffers_give_the_same_answers() {
    let cases = corpus();
    let fresh: Vec<String> = cases
        .iter()
        .map(|c| render(&scan_visible_links_with(&c.screen, c.opts, &Listed(EXISTS))))
        .collect();
    let n = cases.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.extend((0..n).rev());
    order.extend((0..n).flat_map(|i| [i, i]));
    // the largest screens either side of the empty one
    let biggest = |a: usize, b: usize| fresh[a].len().cmp(&fresh[b].len());
    let big = (0..n).max_by(|&a, &b| biggest(a, b)).unwrap();
    order.extend([big, 0, big, 1, big]);

    let mut scratch = LinkScratch::default();
    let mut out = Vec::new();
    for &i in &order {
        let c = &cases[i];
        scan_visible_links_into(&c.screen, c.opts, &Listed(EXISTS), &mut scratch, &mut out);
        assert_eq!(render(&out), fresh[i], "case {:?} after reuse", c.name);
    }
}

/// Whatever is already in `out` is overwritten, not appended to — a
/// stale target in particular must not survive onto a range that has
/// none.
#[test]
fn stale_output_is_replaced() {
    let junk = |n: usize| -> Vec<LinkRange> {
        (0..n)
            .map(|k| LinkRange {
                row: 99,
                col_start: k as u16,
                col_end: 1000,
                kind: LinkKind::Uuid,
                text: "x".repeat(k * 7),
                target: (k % 2 == 0).then(|| "stale/target".repeat(k)),
            })
            .collect()
    };
    for c in corpus() {
        let expect = render(&scan_visible_links_with(&c.screen, c.opts, &Listed(EXISTS)));
        for n in [0, 1, 5, 64] {
            let mut out = junk(n);
            scan_visible_links_into(&c.screen, c.opts, &Listed(EXISTS), &mut LinkScratch::default(), &mut out);
            assert_eq!(render(&out), expect, "case {:?} over {n} stale ranges", c.name);
        }
    }
}

/// More ranges than the std stable sort sorts on the stack, through the
/// table pass's shadow sort.
#[test]
fn a_long_table_sorts_like_the_std_sort() {
    let mut rows = vec![
        "│ a         │ https://raw.githubusercontent.c │".to_string(),
        "│ b         │ om/WeChatCV/opencv_3rdparty/a8b │".to_string(),
    ];
    // every row keeps its rules in the same columns, or it is not one table
    for k in 0..90 {
        rows.push(format!("│ x{k:<8} │ {:<31} │", format!("https://h{k}.example/p a@b.jp")));
    }
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let screen = Screen::new(&refs, 47);
    let opts = ScanOpts { cwd: None, tui_mode: true };
    let expect = scan_visible_links_with(&screen, opts, &Listed(EXISTS));
    assert!(expect.len() > 100, "{}", expect.len());
    let whole = "https://raw.githubusercontent.com/WeChatCV/opencv_3rdparty/a8b";
    assert!(expect.iter().any(|l| l.text == whole), "the table pass must have run");
    let mut sorted = expect.clone();
    sorted.sort_by(|a, b| {
        a.row.cmp(&b.row).then(a.col_start.cmp(&b.col_start)).then(b.text.len().cmp(&a.text.len()))
    });
    assert_eq!(render(&sorted), render(&expect));
    let mut out = Vec::new();
    let mut scratch = LinkScratch::default();
    for _ in 0..2 {
        scan_visible_links_into(&screen, opts, &Listed(EXISTS), &mut scratch, &mut out);
        assert_eq!(render(&out), render(&expect));
    }
}

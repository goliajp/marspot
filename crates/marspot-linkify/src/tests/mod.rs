//! Shared fixtures for the unit tests, and the tests that span every kind of link.

use super::*;

mod hard_wrap;
mod relative_path;
mod seam;

std::thread_local! {
    static REUSED: std::cell::RefCell<(LinkScratch, Vec<LinkRange>)> =
        std::cell::RefCell::new((LinkScratch::default(), Vec::new()));
}

/// Every scan a test makes is repeated on buffers last used by
/// whatever this thread scanned before, and has to agree with the
/// fresh one — so each fixture in this file is also a reuse test.
pub(super) fn check_reuse<S: CellSource>(
    src: &S,
    opts: ScanOpts,
    oracle: &dyn PathOracle,
    fresh: &[LinkRange],
) {
    REUSED.with_borrow_mut(|(scratch, out)| {
        scan_visible_links_into(src, opts, oracle, scratch, out);
        assert_eq!(format!("{out:?}"), format!("{fresh:?}"), "reused buffers changed the result");
    });
}

/// A directory of one test's own under the system temp dir, removed on
/// drop.  A fixed name is shared by every run, worktree and test binary
/// on the machine, and one run's leftovers become the next one's input.
pub(crate) struct ScratchDir(pub std::path::PathBuf);
impl ScratchDir {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let p = std::env::temp_dir().join(format!(
            "marspot-linkify-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).expect("create the test's temp dir");
        Self(p)
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Minimal CellSource over plain rows of text — what a test needs
/// and nothing more.  Wide chars are "everything above ASCII that
/// the terminal would render double-width"; for tests the CJK +
/// fullwidth ranges suffice.
pub(crate) struct StrSource {
    pub rows: Vec<Vec<char>>,
    pub cols: u16,
    pub cursor: (u16, u16),
    pub soft_wrapped: Vec<bool>,
}
impl StrSource {
    pub fn new(rows: &[&str], cols: u16) -> Self {
        let rows: Vec<Vec<char>> = rows
            .iter()
            .map(|r| {
                let mut v: Vec<char> = r.chars().collect();
                v.truncate(cols as usize);
                while v.len() < cols as usize {
                    v.push('\0');
                }
                v
            })
            .collect();
        let n = rows.len();
        Self { rows, cols, cursor: (0, 0), soft_wrapped: vec![false; n] }
    }
}
impl CellSource for StrSource {
    fn cols(&self) -> u16 { self.cols }
    fn rows(&self) -> u16 { self.rows.len() as u16 }
    fn char_at(&self, col: u16, row: u16) -> char {
        self.rows[row as usize][col as usize]
    }
    fn is_soft_wrap_continuation(&self, row: u16) -> bool {
        self.soft_wrapped[row as usize]
    }
    fn cursor(&self) -> (u16, u16) { self.cursor }
    fn is_wide(&self, ch: char) -> bool {
        matches!(ch,
            '\u{1100}'..='\u{115F}' | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}' | '\u{F900}'..='\u{FAFF}'
            | '\u{FE30}'..='\u{FE4F}' | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}' | '\u{1F300}'..='\u{1FAFF}')
    }
}

pub(crate) fn scan(s: &str) -> Vec<LinkRange> {
    let mut out = Vec::new();
    scan_line(s, 0, &mut out);
    out
}

pub(crate) fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

#[test]
fn empty_line_emits_nothing() {
    assert!(scan("").is_empty());
    assert!(scan("                ").is_empty());
}

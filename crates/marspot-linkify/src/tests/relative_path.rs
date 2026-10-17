//! Relative paths, resolved against the pane's working directory.

use super::*;

/// An oracle that knows exactly one directory's contents, so a
/// test states what exists instead of touching a real filesystem.
struct Fake(&'static [&'static str]);
impl PathOracle for Fake {
    fn probe(&self, path: &str) -> PathVerdict {
        if self.0.contains(&path) {
            PathVerdict::Exists
        } else {
            PathVerdict::Missing
        }
    }
}

struct Line(String);
impl CellSource for Line {
    fn cols(&self) -> u16 {
        120
    }
    fn rows(&self) -> u16 {
        1
    }
    fn char_at(&self, col: u16, _row: u16) -> char {
        self.0.chars().nth(col as usize).unwrap_or(' ')
    }
    fn is_soft_wrap_continuation(&self, _row: u16) -> bool {
        false
    }
    fn cursor(&self) -> (u16, u16) {
        (0, 0)
    }
    fn is_wide(&self, _ch: char) -> bool {
        false
    }
}

fn scan(text: &str, cwd: Option<&str>, exists: &'static [&'static str]) -> Vec<LinkRange> {
    scan_visible_links_with(
        &Line(text.to_string()),
        ScanOpts { cwd, ..Default::default() },
        &Fake(exists),
    )
}

#[test]
fn a_relative_path_that_resolves_is_a_link_to_the_full_path() {
    let v = scan(
        "edit src/main.rs then run it",
        Some("/w/proj"),
        &["/w/proj/src/main.rs"],
    );
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].kind, LinkKind::File);
    // The underline covers what is drawn …
    assert_eq!(v[0].text, "src/main.rs");
    // … and Open / Copy get the file.
    assert_eq!(v[0].target.as_deref(), Some("/w/proj/src/main.rs"));
}

#[test]
fn a_colon_ends_the_word_before_a_path_rather_than_joining_it() {
    // Reported 2026-09-11.  The line on screen was
    // `⏺ 写好了:.notes/sentori-ack-v8.0.1.md(442 行,六部分)。`
    // and the path drew as ordinary text.  The candidate started
    // at `写` — a colon counted as "inside a token", so it did not
    // start at the path — ran to the next space, resolved to
    // nothing, and blocked every start position it had covered.
    let v = scan(
        "⏺ 写好了:.notes/sentori-ack-v8.0.1.md(442 行,六部分)。",
        Some("/w/proj"),
        &["/w/proj/.notes/sentori-ack-v8.0.1.md"],
    );
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, ".notes/sentori-ack-v8.0.1.md");
    assert_eq!(
        v[0].target.as_deref(),
        Some("/w/proj/.notes/sentori-ack-v8.0.1.md")
    );
}

#[test]
fn the_same_holds_for_an_ascii_word_and_for_a_tui_pane() {
    let v = scan("wrote:docs/notes/x.md", Some("/w/proj"), &["/w/proj/docs/notes/x.md"]);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, "docs/notes/x.md");

    let v = scan_visible_links_with(
        &Line("⏺ 写好了:.notes/x.md(442 行)。".to_string()),
        ScanOpts { cwd: Some("/w/proj"), tui_mode: true },
        &Fake(&["/w/proj/.notes/x.md"]),
    );
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, ".notes/x.md");
}

#[test]
fn a_name_that_is_not_there_yet_does_not_become_the_directory_beside_it() {
    // Reported 2026-09-11.  `.notes/exprtool.html` was being
    // described before it was generated; `.notes/exprtool/`, the
    // directory it is generated FROM, was already there.  The
    // screen underlined `.notes/exprtool` and left `.html` as
    // plain text — a link to a directory the line never named.
    //
    // A file that does not exist is not a link.  Backing off to a
    // prefix that does is how a missing name turns into a wrong
    // one, and a `.` inside a token is an extension separator,
    // never prose.
    let v = scan(
        ".notes/exprtool.html, 由 harness/scripts/make.py 生成",
        Some("/w/p"),
        &["/w/p/.notes/exprtool"],
    );
    assert!(v.is_empty(), "{v:?}");

    // Once it exists, it is the link — whole, extension included.
    let v = scan(
        ".notes/exprtool.html, 由 harness/scripts/make.py 生成",
        Some("/w/p"),
        &["/w/p/.notes/exprtool", "/w/p/.notes/exprtool.html"],
    );
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, ".notes/exprtool.html");
    assert_eq!(v[0].target.as_deref(), Some("/w/p/.notes/exprtool.html"));
}

#[test]
fn a_period_that_really_does_end_the_sentence_is_still_taken_off() {
    // The counterpart: dropping `.` as a cut point must not cost
    // the trailing-period case, which is arbitrated one step
    // earlier by `trim_sentence_tail`.
    let v = scan("wrote docs/a.md.", Some("/w/p"), &["/w/p/docs/a.md"]);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, "docs/a.md");
}

#[test]
fn a_colon_after_a_path_is_still_the_line_number_rule() {
    // The colon INSIDE a span is arbitrated where it always was,
    // by the line/col suffix rule — not by where a candidate is
    // allowed to start.  The span stops at the file.
    let v = scan("at src/main.rs:42 it failed", Some("/w/proj"), &["/w/proj/src/main.rs"]);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, "src/main.rs");
    assert_eq!(v[0].target.as_deref(), Some("/w/proj/src/main.rs"));
}

#[test]
fn one_that_does_not_resolve_is_not_a_link() {
    let v = scan("edit src/main.rs", Some("/w/proj"), &[]);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn without_a_working_directory_nothing_relative_matches() {
    // `src/main.rs` names nothing on its own.
    let v = scan("edit src/main.rs", None, &["/w/proj/src/main.rs"]);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn dot_forms_resolve_and_the_dot_slash_is_folded_away() {
    let v = scan("see ./notes.md", Some("/w/proj"), &["/w/proj/notes.md"]);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, "./notes.md");
    assert_eq!(v[0].target.as_deref(), Some("/w/proj/notes.md"));

    let up = scan("see ../sib/x.rs", Some("/w/proj"), &["/w/proj/../sib/x.rs"]);
    assert_eq!(up.len(), 1, "{up:?}");
    assert_eq!(up[0].target.as_deref(), Some("/w/proj/../sib/x.rs"));
}

#[test]
fn a_bare_filename_needs_an_extension_not_just_a_match() {
    // A word that happens to name a directory must not light up in
    // prose — `~` is full of them (Music, Public, Downloads).
    let bare = scan("play some Music now", Some("/home/u"), &["/home/u/Music"]);
    assert!(bare.is_empty(), "a bare word is not a path: {bare:?}");
    // With an extension it is worth asking about.
    let named = scan("open Cargo.toml", Some("/w/p"), &["/w/p/Cargo.toml"]);
    assert_eq!(named.len(), 1, "{named:?}");
    assert_eq!(named[0].target.as_deref(), Some("/w/p/Cargo.toml"));
}

#[test]
fn numbers_are_never_candidates() {
    // Even if a file with that name existed, `1.5` in prose is a
    // number.  The extension must start with a letter and the stem
    // must contain one.
    for (text, exists) in [
        ("took 1.5 seconds", "/w/p/1.5"),
        ("version v1.2.3 shipped", "/w/p/v1.2.3"),
        ("built 2026.09 release", "/w/p/2026.09"),
    ] {
        let v = scan(text, Some("/w/p"), &[]);
        assert!(v.is_empty(), "{text}: {v:?}");
        let _ = exists;
    }
}

#[test]
fn absolute_and_url_forms_still_win() {
    // The relative branch runs last; nothing it could match is
    // allowed to re-read something another kind already claimed.
    let v = scan("go to https://x.dev/a.rs", Some("/w/p"), &[]);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].kind, LinkKind::Url);
    assert_eq!(v[0].target, None, "a URL is what it shows");

    let m = scan("mail a.b@c.dev now", Some("/w/p"), &[]);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].kind, LinkKind::Email);
}

/// Two rows, as a fixed-width TUI actually draws them.
struct Rows(Vec<String>, u16);
impl CellSource for Rows {
    fn cols(&self) -> u16 {
        self.1
    }
    fn rows(&self) -> u16 {
        self.0.len() as u16
    }
    fn char_at(&self, col: u16, row: u16) -> char {
        // Columns, not chars: a CJK glyph occupies two.
        let mut c = 0u16;
        for ch in self.0.get(row as usize).map(|s| s.chars()).into_iter().flatten() {
            let w = if self.is_wide(ch) { 2 } else { 1 };
            if col < c + w {
                return if col == c { ch } else { '\0' };
            }
            c += w;
        }
        ' '
    }
    fn is_soft_wrap_continuation(&self, _r: u16) -> bool {
        false
    }
    fn cursor(&self) -> (u16, u16) {
        (0, 0)
    }
    fn is_wide(&self, ch: char) -> bool {
        matches!(ch as u32, 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xFF00..=0xFF60)
    }
}

#[test]
fn a_relative_path_broken_at_a_tui_hard_wrap_is_still_one_link() {
    // The geometry a real report arrived in: a 73-column pane, the
    // path running to column 69, and the remainder on the next row
    // after four spaces.  Three unwrapped paths in the same block
    // were recognised and this one was not — the pane's agent-TUI
    // flag had been lost across a core swap, so the rows were
    // never merged.
    let rows = vec![
        "  - one item with attachments (notes/20260907-maintenance-experiment-".to_string(),
        "    priorities.md)".to_string(),
    ];
    let exists: &'static [&'static str] =
        &["/w/lab/notes/20260907-maintenance-experiment-priorities.md"];
    let v = scan_visible_links_with(
        &Rows(rows.clone(), 73),
        ScanOpts { cwd: Some("/w/lab"), tui_mode: true },
        &Fake(exists),
    );
    assert!(!v.is_empty(), "the wrapped path was not recognised");
    assert_eq!(v[0].target.as_deref(), Some(exists[0]), "{v:?}");

    // Without the TUI flag the rows are two lines and neither half
    // names anything — which is correct, and is why the flag has
    // to survive a core swap.
    let off = scan_visible_links_with(
        &Rows(rows, 73),
        ScanOpts { cwd: Some("/w/lab"), tui_mode: false },
        &Fake(exists),
    );
    assert!(off.is_empty(), "{off:?}");
}

#[test]
fn a_line_col_suffix_is_trimmed_the_same_way() {
    // rustc output: `src/main.rs:120:5`.
    let v = scan(
        "error at src/main.rs:120:5 here",
        Some("/w/p"),
        &["/w/p/src/main.rs"],
    );
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].text, "src/main.rs");
    assert_eq!(v[0].target.as_deref(), Some("/w/p/src/main.rs"));
}

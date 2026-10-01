//! Path 4 of five: rotation hands the hot file over and starts a new
//! one, so a line number means a different line afterwards.
//!
//! Alone in its own binary because it sets
//! `MARSPOT_SCROLLBACK_HOT_CAP_MB`, and an env var is process-wide.
//! Sharing a binary with the other four made every one of them fail:
//! a cap of zero rotates every file immediately, so their marks were
//! handed away before they looked.  The other paths are in
//! `line_identity.rs`.

use marspot_term::grid::PromptMark;
use marspot_term::scrollback::Scrollback;

fn row(c: u8, cols: usize) -> Vec<marspot_term::grid::Cell> {
    (0..cols)
        .map(|_| marspot_term::grid::Cell {
            ch: c as char,
            ..Default::default()
        })
        .collect()
}

/// Path 4 -- rotation. The hot file is handed over whole and a fresh
/// one takes its place with its own epoch, so the sidecar at the hot
/// path does not carry over.
#[test]
fn a_rotation_starts_the_marks_over_with_the_file() {
    let dir = std::env::temp_dir().join(format!("marspot-lineid-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("scrollback.bin");
    let idx = dir.join("scrollback.idx");
    // A tiny hot cap so a handful of lines forces the handover.
    unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0") };
    let mut sb = Scrollback::file(bin, idx, 8, 8).unwrap();
    for i in 0..4 {
        sb.push_line_with_wrapped(&row(b'a' + i, 8), false);
        if i == 1 {
            sb.mark_last_line(PromptMark::PromptStart);
        }
    }
    let first_epoch = sb.epoch();
    for i in 0..40 {
        sb.push_line_with_wrapped(&row(b'A' + (i % 26) as u8, 8), false);
    }
    unsafe { std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB") };
    assert_ne!(
        sb.epoch(),
        first_epoch,
        "the fixture has to actually rotate, or this test proves nothing"
    );
    assert_eq!(
        sb.prompt_at(1),
        PromptMark::None,
        "line 1 belongs to the file that was handed over"
    );
}

/// The hazard the previous test does not reach. After a rotation the
/// old line 1 is in the cold tier, so asking for it answers "no mark"
/// whatever the sidecar holds -- that test passes even when the
/// sidecar is stale. This one asks about a line of the NEW file whose
/// index within it is the same as the marked line's was.
#[test]
fn a_new_line_does_not_inherit_the_mark_at_its_index_in_the_old_file() {
    let dir = std::env::temp_dir().join(format!(
        "marspot-lineid-rotate-inherit-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0") };
    let mut sb =
        Scrollback::file(dir.join("scrollback.bin"), dir.join("scrollback.idx"), 8, 4).unwrap();
    // Local index 1 of the first hot file carries a mark.
    for i in 0..3 {
        sb.push_line_with_wrapped(&row(b'a' + i, 8), false);
        if i == 1 {
            sb.mark_last_line(PromptMark::PromptStart);
        }
    }
    let before = sb.epoch();
    // Force the handover, then write enough lines that local index 1 of
    // the NEW file exists and was never marked.
    for i in 0..20 {
        sb.push_line_with_wrapped(&row(b'A' + (i % 26) as u8, 8), false);
    }
    unsafe { std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB") };
    assert_ne!(sb.epoch(), before, "the fixture has to actually rotate");

    // Every line of the current history, asked one by one: none of them
    // was marked except possibly the one that was, and that one is gone
    // with its file.
    let marked: Vec<usize> = (0..sb.len())
        .filter(|i| sb.prompt_at(*i) != PromptMark::None)
        .collect();
    assert_eq!(
        marked,
        Vec::<usize>::new(),
        "a line of the new file inherited the old file's mark at the same index"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

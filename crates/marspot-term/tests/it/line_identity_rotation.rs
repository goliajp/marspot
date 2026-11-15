//! Path 4 of five: the history changes segments, and a mark goes with
//! its line -- into the older segment, and out of the history with it
//! when the budget lets that segment go -- never onto the line that has
//! the same place in the new one.
//!
//! Alone in its own binary because it sets
//! `MARSPOT_SCROLLBACK_HOT_CAP_MB`, and an env var is process-wide.
//! Sharing a binary with the other four made every one of them fail:
//! a cap of zero rotates every file immediately, so their marks were
//! handed away before they looked.  The other paths are in
//! `line_identity.rs`.

use marspot_term::grid::PromptMark;
use marspot_term::scrollback::Scrollback;

// Every test here sets `MARSPOT_SCROLLBACK_HOT_CAP_MB` and holds the
// environment guard for the whole test, not just the set: under
// `cargo test` the tests are threads of one process, and the first
// version of `a_mark_in_the_cold_tier_is_still_read_after_one_handover`
// counted 11 999 handovers because a neighbour had set the cap to zero.
use marspot_term::test_env::EnvGuard;

fn row(c: u8, cols: usize) -> Vec<marspot_term::grid::Cell> {
    (0..cols)
        .map(|_| marspot_term::grid::Cell {
            ch: c as char,
            ..Default::default()
        })
        .collect()
}

/// Path 4 -- a segment change.  The marked line is in the older segment
/// now, and still has its mark: a mark is in the segment beside its
/// line, not in a file keyed by a position.
#[test]
fn a_segment_change_keeps_the_marks_with_their_lines() {
    let dir = std::env::temp_dir().join(format!("marspot-lineid-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("scrollback.bin");
    let idx = dir.join("scrollback.idx");
    // The smallest segments there are, so a handful of lines changes them.
    let mut env = EnvGuard::new();
    env.set("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0");
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
    env.remove("MARSPOT_SCROLLBACK_HOT_CAP_MB");
    assert_ne!(
        sb.epoch(),
        first_epoch,
        "the fixture has to actually rotate, or this test proves nothing"
    );
    assert_eq!(
        sb.prompt_at(1),
        PromptMark::PromptStart,
        "line 1 went into the older segment with its mark"
    );
}

/// The hazard a mark keyed by position would have: a line of the NEW
/// segment whose place in it is the marked line's place in the old.
#[test]
fn a_new_line_does_not_inherit_the_mark_at_its_index_in_the_old_file() {
    let dir = std::env::temp_dir().join(format!(
        "marspot-lineid-rotate-inherit-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut env = EnvGuard::new();
    env.set("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0");
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
    env.remove("MARSPOT_SCROLLBACK_HOT_CAP_MB");
    assert_ne!(sb.epoch(), before, "the fixture has to actually rotate");

    // Every line of the current history, asked one by one: none of them
    // is marked but the one that was.
    let marked: Vec<usize> = (0..sb.len())
        .filter(|i| sb.prompt_at(*i) != PromptMark::None)
        .collect();
    assert_eq!(
        marked,
        vec![1],
        "a line of the new segment inherited the old segment's mark at the same place"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mark on a line some segments back, still inside the budget, is
/// read from wherever that line is now.
#[test]
fn a_mark_in_the_cold_tier_is_still_read_after_one_handover() {
    let dir = std::env::temp_dir().join(format!("marspot-lineid-cold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // full segments of 1 MiB, the first 128 KiB: 5 000 lines are in
    // two segments, well inside the budget
    let mut env = EnvGuard::new();
    env.set("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1");
    let mut sb =
        Scrollback::file(dir.join("scrollback.bin"), dir.join("scrollback.idx"), 8, 4).unwrap();
    let first_epoch = sb.epoch();
    let mut rotations = 0;
    let mut seen = first_epoch;
    for i in 0..5_000 {
        sb.push_line_with_wrapped(&row(b'a' + (i % 26) as u8, 8), false);
        if i == 10 {
            sb.mark_last_line(PromptMark::CommandEnd(Some(42)));
        }
        if sb.epoch() != seen {
            seen = sb.epoch();
            rotations += 1;
        }
    }
    env.remove("MARSPOT_SCROLLBACK_HOT_CAP_MB");
    assert!(rotations >= 1, "the fixture has to change segments");
    assert_eq!(
        sb.prompt_at(10),
        PromptMark::CommandEnd(Some(42)),
        "line 10 is in an older segment and its mark went with it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same, for the cluster text: a line in an older segment keeps
/// what its cells said.
#[test]
fn cluster_text_in_the_cold_tier_is_still_read_after_one_handover() {
    let dir = std::env::temp_dir().join(format!("marspot-clusters-cold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut env = EnvGuard::new();
    env.set("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1");
    let mut sb =
        Scrollback::file(dir.join("scrollback.bin"), dir.join("scrollback.idx"), 8, 4).unwrap();
    let flag = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}".to_string();
    let first_epoch = sb.epoch();
    let mut rotations = 0;
    let mut seen = first_epoch;
    for i in 0..5_000 {
        sb.push_line_with_wrapped(&row(b'a' + (i % 26) as u8, 8), false);
        if i == 10 {
            sb.cluster_last_line(&[(3u16, flag.clone())]);
        }
        if sb.epoch() != seen {
            seen = sb.epoch();
            rotations += 1;
        }
    }
    env.remove("MARSPOT_SCROLLBACK_HOT_CAP_MB");
    assert!(rotations >= 1, "the fixture has to change segments");
    let mut got = Vec::new();
    assert!(
        sb.clusters_at(10, &mut got),
        "line 10 is in an older segment and its cluster text went with it"
    );
    assert_eq!(got, vec![(3u16, flag)]);
    // And no other line picks it up.
    got.clear();
    assert!(
        !sb.clusters_at(4_999, &mut got),
        "the last line never had a cluster"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

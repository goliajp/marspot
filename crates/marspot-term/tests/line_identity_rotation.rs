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

/// Every test here sets `MARSPOT_SCROLLBACK_HOT_CAP_MB`, and an env var
/// is process-wide: under `cargo test` these run as threads in one
/// process, so without this they read each other's cap.  The first
/// version of `a_mark_in_the_cold_tier_is_still_read_after_one_handover`
/// wanted one handover and counted 11 999, because a neighbour had set
/// the cap to zero.  Held for the whole of each test, not just the
/// `set_var`.
static CAP: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _cap = CAP.lock().unwrap_or_else(|p| p.into_inner());
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
    let _cap = CAP.lock().unwrap_or_else(|p| p.into_inner());
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

/// What the rename is for: after ONE handover, the marks of the lines
/// that went with the file are still readable through the cold tier.
///
/// The two tests above use a cap of zero, which rotates on every push
/// and so overwrites the cold pair immediately -- they say nothing
/// about whether cold marks work. This one rotates once.
#[test]
fn a_mark_in_the_cold_tier_is_still_read_after_one_handover() {
    let dir = std::env::temp_dir().join(format!("marspot-lineid-cold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 1 MiB of hot file is a few thousand 8-column records, so one pass
    // below hands the file over exactly once.
    let _cap = CAP.lock().unwrap_or_else(|p| p.into_inner());
    unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1") };
    let mut sb =
        Scrollback::file(dir.join("scrollback.bin"), dir.join("scrollback.idx"), 8, 4).unwrap();
    let first_epoch = sb.epoch();
    let mut rotations = 0;
    let mut seen = first_epoch;
    for i in 0..12_000 {
        sb.push_line_with_wrapped(&row(b'a' + (i % 26) as u8, 8), false);
        if i == 10 {
            sb.mark_last_line(PromptMark::CommandEnd(Some(42)));
        }
        if sb.epoch() != seen {
            seen = sb.epoch();
            rotations += 1;
        }
    }
    unsafe { std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB") };
    assert_eq!(
        rotations, 1,
        "the fixture needs exactly one handover, or it is testing something else"
    );
    assert_eq!(
        sb.prompt_at(10),
        PromptMark::CommandEnd(Some(42)),
        "line 10 went into the cold tier and its mark went with it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same handover, for the cluster text: a line that went cold keeps
/// what its cells said, because every sidecar of that `.bin` was renamed
/// beside it rather than only the marks.
///
/// Worth its own test rather than trusting the shared rename, because
/// the first version of the cold-tier mark test was green for the wrong
/// reason -- with a cap of zero every line rotates immediately, so
/// "no record" was the right answer whatever the code did.
#[test]
fn cluster_text_in_the_cold_tier_is_still_read_after_one_handover() {
    let dir = std::env::temp_dir().join(format!("marspot-clusters-cold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _cap = CAP.lock().unwrap_or_else(|p| p.into_inner());
    unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1") };
    let mut sb =
        Scrollback::file(dir.join("scrollback.bin"), dir.join("scrollback.idx"), 8, 4).unwrap();
    let flag = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}".to_string();
    let first_epoch = sb.epoch();
    let mut rotations = 0;
    let mut seen = first_epoch;
    for i in 0..12_000 {
        sb.push_line_with_wrapped(&row(b'a' + (i % 26) as u8, 8), false);
        if i == 10 {
            sb.cluster_last_line(&[(3u16, flag.clone())]);
        }
        if sb.epoch() != seen {
            seen = sb.epoch();
            rotations += 1;
        }
    }
    unsafe { std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB") };
    assert_eq!(
        rotations, 1,
        "the fixture needs exactly one handover, or it is testing something else"
    );
    let mut got = Vec::new();
    assert!(
        sb.clusters_at(10, &mut got),
        "line 10 went into the cold tier and its cluster text went with it"
    );
    assert_eq!(got, vec![(3u16, flag)]);
    // And a line of the new hot file does not pick it up at the index
    // it had in the old one.
    got.clear();
    assert!(
        !sb.clusters_at(11_999, &mut got),
        "the last line never had a cluster"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

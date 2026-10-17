//! The whole path a pane actually takes: bytes in, cluster in history.
//!
//! `cluster_history.rs` builds the cells itself. This one feeds real
//! UTF-8 through the parser into a `Terminal` that chose its own
//! file-backed scrollback the way an L3 session does -- from
//! `MARSPOT_SESSION_ID` -- then drops it, reopens it, and reads the
//! cluster back. What it adds over the other file is the two ends:
//! that the parser's clusters reach the sidecar at all, and that the
//! selection of a file-backed scrollback is the production one rather
//! than a constructor a test called.
//!
//! It sets environment variables, which are process-wide, so each test
//! holds the environment guard while it runs.

use marspot_term::terminal::Terminal;

/// A state dir and session id of its own, set the way L3 sets them.
fn env(sid: u64) -> marspot_term::test_env::StateSandbox {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    std::fs::create_dir_all(sandbox.dir.join("sessions").join(sid.to_string())).unwrap();
    sandbox.env.set("MARSPOT_SESSION_ID", sid.to_string());
    sandbox
}

/// Every cluster the viewport can see at this offset, read the way the
/// renderer reads it.
fn clusters_on_screen(t: &Terminal) -> Vec<String> {
    let g = t.grid();
    let mut out = Vec::new();
    let mut row_clusters = Vec::new();
    for off in 0..=g.scrollback_len() as u16 {
        for row in 0..g.rows() {
            g.row_clusters_at_view(off, row, &mut row_clusters);
            for col in 0..g.cols() {
                let cell = g.cell_at_view(off, col, row);
                if let Some(t) = g
                    .cluster_text(&cell)
                    .or_else(|| marspot_term::grid::cluster_in_row(&row_clusters, col))
                {
                    out.push(t.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn clusters_fed_as_bytes_come_back_after_the_session_restarts() {
    let _env = env(90_001);
    let combining = "e\u{301}";
    let flag = "\u{1F1EF}\u{1F1F5}";
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

    let history_len;
    {
        let mut t = Terminal::new(40, 6);
        assert!(
            t.grid().scrollback_len() == 0,
            "a fresh session starts with no history"
        );
        t.feed(format!("{combining} {flag} {family}\r\n").as_bytes());
        // Push it off the screen and well past the viewport, so what is
        // read back comes from the file rather than from the rows the
        // grid still holds.
        for i in 0..40 {
            t.feed(format!("filler {i}\r\n").as_bytes());
        }
        let seen = clusters_on_screen(&t);
        for want in [combining, flag, family] {
            assert!(
                seen.iter().any(|s| s == want),
                "the pane that printed {want:?} can read it back: {seen:?}"
            );
        }
        history_len = t.grid().scrollback_len();
        assert!(
            history_len > 6,
            "the fixture has to push the line past the screen, got {history_len}"
        );
    }

    // A new process over the same files: nothing in this `Terminal` ever
    // saw the bytes.
    let t = Terminal::new(40, 6);
    assert_eq!(
        t.grid().scrollback_len(),
        history_len,
        "the history came from the file, not from anything this process did"
    );
    let seen = clusters_on_screen(&t);
    for want in [combining, flag, family] {
        assert!(
            seen.iter().any(|s| s == want),
            "a restarted session reads {want:?} out of the sidecar: {seen:?}"
        );
    }
}

/// And a session that printed no clusters files nothing -- the sidecar
/// of a plain-text pane stays at its header, which is what makes the
/// common case free.
#[test]
fn a_pane_of_plain_text_files_nothing() {
    let _env = env(90_002);
    let mut t = Terminal::new(40, 6);
    for i in 0..60 {
        t.feed(format!("plain line {i}\r\n").as_bytes());
    }
    drop(t);
    let dir = std::env::var("MARSPOT_STATE_DIR").unwrap();
    let text = std::path::Path::new(&dir)
        .join("sessions")
        .join("90002")
        .join("scrollback.bin.clustertext");
    let len = std::fs::metadata(&text).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        len,
        24,
        "no clusters means no records, only the header: {} is {len} bytes",
        text.display()
    );
}

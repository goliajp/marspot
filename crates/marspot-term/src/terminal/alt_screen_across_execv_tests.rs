use super::Terminal;

/// A pane inside a full-screen program must still know it is
/// there after an image swap.
///
/// Two snapshot forms exist and only one is right here: the fold
/// form deliberately folds an alt screen into main (it is for
/// persistence, where the shell's history is what matters), while
/// `serialize_snapshot_live` is the execv handoff and carries the
/// alt screen verbatim.  Reading the wrong one reports the state
/// as lost, which is how it nearly got "fixed" — everything keyed
/// on `FLAG_ALT_SCREEN` would then be keyed on an artefact.
#[test]
fn the_alternate_screen_survives_a_snapshot() {
    let mut before = Terminal::new(20, 4);
    before.feed(b"\x1b[?1049hinside the TUI");
    assert!(before.in_alt_screen());

    let mut after = Terminal::new(20, 4);
    after
        .apply_snapshot(&before.serialize_snapshot_live())
        .unwrap();
    assert!(
        after.in_alt_screen(),
        "the program is still in its own screen; the swap was ours, not its"
    );
}

/// A pane that was NOT in one must not be told it was.
#[test]
fn an_ordinary_pane_is_not_moved_into_one() {
    let mut before = Terminal::new(20, 4);
    before.feed(b"$ ls");
    let mut after = Terminal::new(20, 4);
    after
        .apply_snapshot(&before.serialize_snapshot_live())
        .unwrap();
    assert!(!after.in_alt_screen());
}

/// Leaving works, and lands on a blank main screen — the honest
/// outcome, since the main grid was never in the snapshot.  The
/// alternative, staying put, leaves the program's own painting
/// behind as the shell's history.
#[test]
fn leaving_after_a_swap_does_not_keep_the_programs_painting() {
    let mut before = Terminal::new(20, 4);
    before.feed(b"\x1b[?1049hTUI PAINTED THIS");
    let mut after = Terminal::new(20, 4);
    after
        .apply_snapshot(&before.serialize_snapshot_live())
        .unwrap();

    let painted: String = (0..20).map(|c| after.grid().cell(c, 0).ch).collect();
    assert!(
        painted.contains("TUI"),
        "the alternate screen is what we restored"
    );

    after.feed(b"\x1b[?1049l");
    assert!(!after.in_alt_screen());
    let main: String = (0..20).map(|c| after.grid().cell(c, 0).ch).collect();
    assert!(
        !main.contains("TUI"),
        "the program's screen must not become the shell's: {main:?}"
    );
}

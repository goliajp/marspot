use super::Terminal;

/// DEC 2026 opens and closes a batch.  codex brackets every frame
/// with it; treating it as a no-op is what let half-drawn screens
/// reach the display.
#[test]
fn dec_2026_opens_and_closes_a_synchronized_update() {
    let mut t = Terminal::new(20, 4);
    assert!(!t.sync_output_active());
    t.feed(b"\x1b[?2026h");
    assert!(t.sync_output_active());
    t.feed(b"hello");
    assert!(
        t.sync_output_active(),
        "content inside the batch keeps it open"
    );
    t.feed(b"\x1b[?2026l");
    assert!(!t.sync_output_active());
}

/// A program that dies mid-update must not hold the next one's
/// first frame hostage.
#[test]
fn a_process_handover_clears_a_dangling_update() {
    let mut t = Terminal::new(20, 4);
    t.feed(b"\x1b[?2026h");
    assert!(t.sync_output_active());
    t.reset_process_owned_modes();
    assert!(!t.sync_output_active());
}

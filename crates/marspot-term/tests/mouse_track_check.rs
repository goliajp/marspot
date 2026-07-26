// Verify DEC mode 1000/1002/1003/1006 now reach Terminal state
// (commit 4429e0d wired them from stubbed no-op).

use marspot_term::terminal::{MouseTrackingMode, Terminal};

#[test]
fn dec_1000_enables_x11_mouse() {
    let mut t = Terminal::new(20, 5);
    assert_eq!(t.mouse_tracking_mode(), MouseTrackingMode::Off);
    t.feed(b"\x1b[?1000h");
    assert_eq!(t.mouse_tracking_mode(), MouseTrackingMode::X11);
    t.feed(b"\x1b[?1000l");
    assert_eq!(t.mouse_tracking_mode(), MouseTrackingMode::Off);
}

#[test]
fn dec_1003_enables_any_event_mouse() {
    let mut t = Terminal::new(20, 5);
    t.feed(b"\x1b[?1003h");
    assert_eq!(t.mouse_tracking_mode(), MouseTrackingMode::AnyEvent);
}

#[test]
fn dec_1006_enables_sgr_encoding() {
    let mut t = Terminal::new(20, 5);
    assert!(!t.mouse_sgr_encoding());
    t.feed(b"\x1b[?1006h");
    assert!(t.mouse_sgr_encoding());
    t.feed(b"\x1b[?1006l");
    assert!(!t.mouse_sgr_encoding());
}

#[test]
fn mouse_state_survives_snapshot_round_trip() {
    // Reproduce the "只有少数 work" symptom — across L3 execv the
    // snapshot must carry mouse_tracking_mode + sgr_encoding.
    let mut t1 = Terminal::new(20, 5);
    t1.feed(b"\x1b[?1003h\x1b[?1006h");
    assert_eq!(t1.mouse_tracking_mode(), MouseTrackingMode::AnyEvent);
    assert!(t1.mouse_sgr_encoding());

    let body = t1.serialize_snapshot();
    let mut t2 = Terminal::new(20, 5);
    t2.apply_snapshot(&body).expect("apply");
    assert_eq!(t2.mouse_tracking_mode(), MouseTrackingMode::AnyEvent);
    assert!(t2.mouse_sgr_encoding());
}

/// A resurrected session restores its snapshot onto a **brand-new
/// shell**.  The content is the point of the restore; the modes are
/// not — they were switched on by a program that is gone.
///
/// Left set, the terminal lies about the fresh shell in a way the user
/// sees immediately: with tracking "on", marspot encodes a scroll as
/// `CSI < 64;x;y M` and types it at the zsh prompt, which answers
/// `command not found: 29M64` for every wheel notch.
#[test]
fn cold_resurrection_drops_modes_but_keeps_content() {
    let mut old_life = Terminal::new(20, 5);
    // A TUI ran here: mouse tracking + SGR + bracketed paste + app
    // cursor keys, and it hid the cursor.
    old_life.feed(b"\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b[?25l");
    old_life.feed(b"scrollback line\r\n");
    assert_eq!(old_life.mouse_tracking_mode(), MouseTrackingMode::AnyEvent);

    let body = old_life.serialize_snapshot();

    // The resurrection path: fresh Terminal, snapshot applied, then
    // the process-owned modes dropped.
    let mut new_life = Terminal::new(20, 5);
    new_life.apply_snapshot(&body).expect("apply");
    new_life.reset_process_owned_modes();

    assert_eq!(
        new_life.mouse_tracking_mode(),
        MouseTrackingMode::Off,
        "a scroll must not be encoded as a mouse report to a shell \
         that never asked for one"
    );
    assert!(!new_life.mouse_sgr_encoding());
    assert!(!new_life.bracketed_paste_mode());
    assert!(!new_life.cursor_key_application_mode());
    assert!(new_life.cursor_visible(), "a new shell shows its cursor");

    // …and the content the restore exists for is still there.
    let g = new_life.grid();
    let mut text = String::new();
    for r in 0..g.rows() {
        for c in 0..20u16 {
            text.push(g.cell(c, r).ch);
        }
        text.push('\n');
    }
    assert!(
        text.contains("scrollback line"),
        "restore must keep the screen it was made for; got {text:?}"
    );
}

/// The execv handoff is the opposite case: the shell survives, so
/// every one of those modes is still genuinely set and must persist.
/// This is what `mouse_state_survives_snapshot_round_trip` guards —
/// stated here too so the two paths can't be conflated by a future
/// "just reset it everywhere" change.
#[test]
fn execv_handoff_keeps_modes() {
    let mut before = Terminal::new(20, 5);
    before.feed(b"\x1b[?1003h\x1b[?1006h");
    let body = before.serialize_snapshot();

    let mut after = Terminal::new(20, 5);
    after.apply_snapshot(&body).expect("apply");
    // No reset here — the shell is the same process.
    assert_eq!(after.mouse_tracking_mode(), MouseTrackingMode::AnyEvent);
    assert!(after.mouse_sgr_encoding());
}

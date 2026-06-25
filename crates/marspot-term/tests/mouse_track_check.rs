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

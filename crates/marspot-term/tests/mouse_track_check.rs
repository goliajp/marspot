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

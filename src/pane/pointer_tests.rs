use super::*;

fn sgr(button: u8, pressed: bool, col: u32, row: u32) -> String {
    let mut b = Vec::new();
    PaneBackend::encode_mouse(&mut b, true, button, pressed, col, row);
    String::from_utf8(b).unwrap()
}

fn x10(button: u8, pressed: bool, col: u32, row: u32) -> Vec<u8> {
    let mut b = Vec::new();
    PaneBackend::encode_mouse(&mut b, false, button, pressed, col, row);
    b
}

/// SGR says which button came up.  X10 cannot, and says 3.
///
/// That difference is the whole reason a program asks for `?1006`,
/// and getting it backwards means a program either never sees the
/// release or thinks a different button was let go.
#[test]
fn a_release_is_encoded_the_way_each_form_can_say_it() {
    assert_eq!(sgr(0, true, 12, 34), "\u{1b}[<0;12;34M");
    assert_eq!(sgr(0, false, 12, 34), "\u{1b}[<0;12;34m");
    assert_eq!(sgr(2, false, 1, 1), "\u{1b}[<2;1;1m");

    assert_eq!(x10(0, true, 12, 34), b"\x1b[M\x20\x2c\x42".to_vec());
    // 3 + 32 = 35 = b'#': any button, released.
    assert_eq!(x10(0, false, 12, 34)[3], b'#');
    assert_eq!(x10(2, false, 12, 34)[3], b'#', "X10 cannot name the button");
}

/// The X10 form runs out of room at column 223.
///
/// It packs each coordinate into one byte at +32, so 223 is the
/// last column it can name.  Clamping is the encoding's ceiling,
/// not a decision taken here — but it has to be a clamp and not a
/// wrap, or column 224 would report as column 1.
#[test]
fn the_legacy_form_clamps_rather_than_wraps() {
    let far = x10(0, true, 500, 500);
    assert_eq!(far[4], 223 + 32);
    assert_eq!(far[5], 223 + 32);
    // And SGR, which has no such limit, says the real number.
    assert_eq!(sgr(0, true, 500, 500), "\u{1b}[<0;500;500M");
}

/// Motion carries the 32 bit, and a held button rides under it.
///
/// `?1002` reports travel while a button is held and `?1003`
/// reports all of it; the difference on the wire is which button
/// is named, with 3 meaning none.  Both encodings add 32 to say
/// this is travel rather than a press, which is why the legacy
/// byte lands at button + 64.
#[test]
fn motion_is_a_button_plus_the_motion_bit() {
    // Left button (0) held while moving: the motion bit makes 32.
    assert_eq!(sgr(32, true, 7, 9), "\u{1b}[<32;7;9M");
    // No button (3) while moving: 35.
    assert_eq!(sgr(35, true, 7, 9), "\u{1b}[<35;7;9M");
    // X10 adds its own 32 on top of whatever it is handed.
    assert_eq!(x10(32, true, 7, 9)[3], 64);
    assert_eq!(x10(35, true, 7, 9)[3], 67);
}

/// A wheel tick is a press, never a release.
#[test]
fn a_wheel_tick_is_a_press() {
    assert!(sgr(64, true, 5, 5).ends_with('M'));
    assert_eq!(sgr(65, true, 5, 5), "\u{1b}[<65;5;5M");
}

fn ev(kind: PointerKind, button: u8, count: u16) -> PointerInput {
    PointerInput {
        kind,
        button,
        count,
        col: 3,
        row: 1,
        keys: b"\x1b[5~".to_vec(),
    }
}

const SGR: FrameModes = FrameModes {
    sgr: true,
    app_cursor: false,
};

/// A session that checks pointer events is sent the event, not bytes.
#[test]
fn a_session_that_checks_is_sent_the_event_and_decodes_it_whole() {
    for kind in [
        PointerKind::Press,
        PointerKind::Release,
        PointerKind::Motion,
        PointerKind::Wheel,
        PointerKind::Arrows,
        PointerKind::ProgramKeys,
    ] {
        let e = ev(kind, 3, 2);
        let f = pointer_frame(7, true, &e, SGR);
        assert_eq!(f.msg_type, MsgType::PanePointerInput);
        let (sid, back) = marspot_term::shell_proto::decode_pane_pointer_input(&f.payload).unwrap();
        assert_eq!((sid, back), (7, e));
    }
}

/// An older session gets today's bytes, encoded from the frame's modes.
#[test]
fn an_older_session_is_sent_the_bytes_it_always_got() {
    let bytes = |e: &PointerInput, m: FrameModes| {
        let f = pointer_frame(7, false, e, m);
        assert_eq!(f.msg_type, MsgType::InjectInput);
        crate::shell_proto::decode_inject_input(&f.payload)
            .unwrap()
            .1
    };
    assert_eq!(bytes(&ev(PointerKind::Motion, 3, 1), SGR), b"\x1b[<35;3;1M");
    assert_eq!(bytes(&ev(PointerKind::Press, 0, 1), SGR), b"\x1b[<0;3;1M");
    assert_eq!(bytes(&ev(PointerKind::Release, 0, 1), SGR), b"\x1b[<0;3;1m");
    assert_eq!(
        bytes(&ev(PointerKind::Wheel, 65, 2), SGR),
        b"\x1b[<65;3;1M\x1b[<65;3;1M"
    );
    assert_eq!(bytes(&ev(PointerKind::Arrows, 64, 2), SGR), b"\x1b[A\x1b[A");
    let app = FrameModes {
        sgr: false,
        app_cursor: true,
    };
    assert_eq!(bytes(&ev(PointerKind::Arrows, 65, 1), app), b"\x1bOB");
    assert_eq!(
        bytes(&ev(PointerKind::Motion, 3, 1), app),
        b"\x1b[M\x43\x23\x21"
    );
    assert_eq!(bytes(&ev(PointerKind::ProgramKeys, 0, 1), SGR), b"\x1b[5~");
}

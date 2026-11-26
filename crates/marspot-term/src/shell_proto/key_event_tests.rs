use crate::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};
use crate::key_text::KeyText;
use crate::shell_proto::*;

fn encoded(ev: &WireKeyEvent, window_id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key_event(ev, window_id, &mut out);
    out
}

#[test]
fn mods_roundtrip() {
    for b in 0u8..16 {
        let (s, c, a, u) = mods_from_byte(b);
        assert_eq!(mods_to_byte(s, c, a, u), b);
    }
}

#[test]
fn key_event_char_roundtrip() {
    let ev = WireKeyEvent {
        state: WireKeyState::Pressed,
        mods: mods_to_byte(false, true, false, false),
        kind: WireLogicalKind::Char,
        key_data: 'a' as u32,
        text: "a".into(),
        ..Default::default()
    };
    let p = encoded(&ev, 7);
    let (back, win) = decode_key_event(&p).unwrap();
    assert_eq!(win, 7);
    assert_eq!(back.state, ev.state);
    assert_eq!(back.mods, ev.mods);
    assert_eq!(back.kind, ev.kind);
    assert_eq!(back.key_data, ev.key_data);
    assert_eq!(back.text, ev.text);
}

#[test]
fn key_event_named_arrow_roundtrip() {
    let ev = WireKeyEvent {
        state: WireKeyState::Pressed,
        mods: 0,
        kind: WireLogicalKind::Named,
        key_data: WireNamedKey::ArrowUp as u32,
        text: KeyText::EMPTY,
        ..Default::default()
    };
    let p = encoded(&ev, FIRST_WINDOW_ID);
    let (back, _win) = decode_key_event(&p).unwrap();
    assert_eq!(back.kind, WireLogicalKind::Named);
    assert_eq!(
        WireNamedKey::from_u8(back.key_data as u8),
        Some(WireNamedKey::ArrowUp)
    );
    assert_eq!(back.text, "");
}

#[test]
fn a_key_frame_carries_repeat_and_position_both_ways() {
    let ev = WireKeyEvent {
        state: WireKeyState::Pressed,
        mods: mods_to_byte(true, false, false, false),
        kind: WireLogicalKind::Char,
        key_data: 'w' as u32,
        text: "W".into(),
        repeat: true,
        base_layout: 'z' as u32,
    };
    let p = encoded(&ev, 9);
    let (got, win) = decode_key_event(&p).unwrap();
    assert_eq!(win, 9);
    assert!(got.repeat);
    assert_eq!(got.base_layout, 'z' as u32);
    assert_eq!(got.key_data, 'w' as u32);
    assert_eq!(got.text, "W");
}

/// A frame from a writer that predates the two fields.
///
/// This is the shape that has bitten this codebase before: a
/// reader that treats an older frame as malformed takes the whole
/// channel down mid-update.  Built here by hand rather than by
/// calling the encoder, because the encoder can only produce the
/// current shape and a test that asks it to is testing nothing.
fn old_style_key_frame(text: &str, window_id: u32) -> Vec<u8> {
    let t = text.as_bytes();
    let mut out = vec![WireKeyState::Pressed as u8, 0, WireLogicalKind::Char as u8];
    out.push(0); // padding, before it meant anything
    out.extend_from_slice(&('a' as u32).to_le_bytes());
    out.extend_from_slice(&(t.len() as u16).to_le_bytes());
    out.extend_from_slice(t);
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

#[test]
fn a_frame_from_before_the_new_fields_still_decodes() {
    let p = old_style_key_frame("a", 3);
    let (got, win) = decode_key_event(&p).unwrap();
    assert_eq!(win, 3, "the window id is still where it was");
    assert!(!got.repeat, "old padding reads as 'not a repeat'");
    assert_eq!(got.base_layout, 0, "and as 'position unknown'");
    assert_eq!(got.text, "a");
}

#[test]
fn a_reader_from_before_the_new_fields_still_finds_the_window_id() {
    // The other direction, which no current code path can check
    // for itself: a peer running the older image reads the window
    // id from a fixed offset and stops.  Appending after it has
    // to leave that offset holding the same four bytes.
    let ev = WireKeyEvent {
        kind: WireLogicalKind::Char,
        key_data: 'a' as u32,
        text: "a".into(),
        base_layout: 'z' as u32,
        ..Default::default()
    };
    let new = encoded(&ev, 11);
    let old = old_style_key_frame("a", 11);
    assert_eq!(
        &new[..old.len()],
        &old[..],
        "everything an older reader looks at is byte-identical"
    );
    // And what an older reader would compute for the window id.
    let text_len = u16::from_le_bytes(new[8..10].try_into().unwrap()) as usize;
    let at = 10 + text_len;
    assert_eq!(u32::from_le_bytes(new[at..at + 4].try_into().unwrap()), 11);
}

#[test]
fn the_event_conversion_keeps_both_fields() {
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Char('w'),
        text: Some("W".into()),
        repeat: true,
        base_layout: Some('z'),
    };
    let mods = Modifiers {
        shift: true,
        ..Default::default()
    };
    let (back, back_mods) = wire_to_event(event_to_wire(&ev, mods));
    assert_eq!(back.repeat, ev.repeat);
    assert_eq!(back.base_layout, ev.base_layout);
    assert_eq!(back.logical, ev.logical);
    assert_eq!(back_mods, mods);
}

/// The encoder as it stood before it wrote into a caller's buffer,
/// kept verbatim so the bytes on the wire are checked against it.
fn encode_key_event_before(ev: &WireKeyEvent, window_id: u32) -> Vec<u8> {
    let text_bytes = ev.text.as_bytes();
    let mut out = Vec::with_capacity(14 + text_bytes.len() + 4);
    out.push(ev.state as u8);
    out.push(ev.mods);
    out.push(ev.kind as u8);
    out.push(if ev.repeat { 0b0000_0001 } else { 0 });
    out.extend_from_slice(&ev.key_data.to_le_bytes());
    out.extend_from_slice(&(text_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(text_bytes);
    out.extend_from_slice(&window_id.to_le_bytes());
    out.extend_from_slice(&ev.base_layout.to_le_bytes());
    out
}

fn sample_events() -> Vec<(MarspotKeyEvent, Modifiers)> {
    let long = "这是一段通过输入法一次提交的长句子，比内联的容量长";
    let ev = |logical, text: Option<&str>| MarspotKeyEvent {
        state: KeyState::Pressed,
        logical,
        text: text.map(KeyText::from),
        ..Default::default()
    };
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    let ctrl = Modifiers {
        control: true,
        ..Default::default()
    };
    vec![
        (ev(LogicalKey::Char('a'), Some("a")), Modifiers::default()),
        (ev(LogicalKey::Char('a'), Some("A")), shift),
        (ev(LogicalKey::Char('c'), Some("\u{3}")), ctrl),
        (
            ev(LogicalKey::Named(NamedKey::ArrowUp), None),
            Modifiers::default(),
        ),
        (ev(LogicalKey::Named(NamedKey::F12), None), shift),
        (ev(LogicalKey::Other, Some("日本語")), Modifiers::default()),
        (ev(LogicalKey::Other, Some(long)), Modifiers::default()),
        (
            MarspotKeyEvent {
                state: KeyState::Released,
                logical: LogicalKey::Char('z'),
                text: Some("z".into()),
                repeat: true,
                base_layout: Some('w'),
            },
            Modifiers {
                alt: true,
                super_: true,
                ..Default::default()
            },
        ),
    ]
}

/// The bytes on the wire did not change with the buffer-writing
/// encoders: each sample encodes exactly as the old encoder did, by
/// both new paths, and one frame is pinned byte by byte.
#[test]
fn key_frames_are_byte_identical_to_the_old_encoder() {
    let mut buf = Vec::new();
    for (ev, mods) in sample_events() {
        let wire = event_to_wire(&ev, mods);
        let old = encode_key_event_before(&wire, 5);
        buf.clear();
        encode_key_event(&wire, 5, &mut buf);
        assert_eq!(buf, old, "encode_key_event changed the bytes for {ev:?}");
        buf.clear();
        encode_event(&ev, mods, 5, &mut buf);
        assert_eq!(buf, old, "encode_event changed the bytes for {ev:?}");
        let (back, win) = decode_key_event(&old).unwrap();
        assert_eq!((back.text.as_str(), win), (wire.text.as_str(), 5));
    }
    let (ev, mods) = &sample_events()[1];
    buf.clear();
    encode_event(ev, *mods, 2, &mut buf);
    assert_eq!(
        buf,
        [
            0, 1, 0, 0, b'a', 0, 0, 0, 1, 0, b'A', 2, 0, 0, 0, 0, 0, 0, 0
        ],
        "shift+a, window 2"
    );
}

/// A long IME commit crosses whole, and only it goes to the heap.
#[test]
fn a_long_commit_is_delivered_whole() {
    let samples = sample_events();
    let (ev, mods) = &samples[6];
    let mut buf = Vec::new();
    encode_event(ev, *mods, 1, &mut buf);
    let (w, _) = decode_key_event(&buf).unwrap();
    assert!(!w.text.is_inline());
    let (back, _) = wire_to_event(w);
    assert_eq!(back.text, ev.text);
    let (short, _) = decode_key_event(&encoded(&event_to_wire(&samples[5].0, *mods), 1)).unwrap();
    assert!(short.text.is_inline());
}

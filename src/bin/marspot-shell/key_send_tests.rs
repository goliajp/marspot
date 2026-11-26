//! A key from `key_event` to the frame written to the core allocates
//! nothing once warm.

use super::*;
use crate::core_conn::CoreConn;
use marspot::alloc_count::{Counting, counted};
use marspot::input::{KeyText, LogicalKey, NamedKey};
use marspot::shell_proto::{Frame, FrameReader, MsgType, decode_key_event, wire_to_event};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Instant;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn conn_to(stream: UnixStream) -> CoreConn {
    let now = Instant::now();
    CoreConn {
        child: None,
        control_tx: Mutex::new(stream),
        control_rx: std::sync::mpsc::channel().1,
        hello_acked: true,
        paced: true,
        early_ready: Vec::new(),
        spawned_at: now,
        next_ping_at: now,
        last_ping_nonce: 0,
        last_pong_at: now,
    }
}

fn keys() -> Vec<(MarspotKeyEvent, Modifiers)> {
    let press = |logical, text: Option<&str>| MarspotKeyEvent {
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
    let cmd = Modifiers {
        super_: true,
        ..Default::default()
    };
    let long = "这是一段通过输入法一次提交的长句子，比内联的容量长";
    vec![
        (
            press(LogicalKey::Char('a'), Some("a")),
            Modifiers::default(),
        ),
        (press(LogicalKey::Char('a'), Some("A")), shift),
        (press(LogicalKey::Char('c'), Some("\u{3}")), ctrl),
        (press(LogicalKey::Char('c'), Some("c")), cmd),
        (
            press(LogicalKey::Named(NamedKey::Enter), None),
            Modifiers::default(),
        ),
        (
            press(LogicalKey::Named(NamedKey::ArrowLeft), None),
            Modifiers::default(),
        ),
        (
            press(LogicalKey::Other, Some("日本語")),
            Modifiers::default(),
        ),
        (press(LogicalKey::Other, Some(long)), Modifiers::default()),
        (
            MarspotKeyEvent {
                state: KeyState::Released,
                logical: LogicalKey::Char('a'),
                text: Some("a".into()),
                repeat: true,
                base_layout: Some('a'),
            },
            Modifiers::default(),
        ),
    ]
}

/// What `ShellApp::key_event` does with a key that is not the
/// new-window command, minus the AppKit context it is handed.
fn key_event(conn: &CoreConn, buf: &mut Vec<u8>, ev: &MarspotKeyEvent, mods: Modifiers) {
    if opens_window(ev, mods) {
        return;
    }
    crate::pane_control::key_pressed();
    encode_key(buf, ev, mods, 3);
    assert!(conn.send(MsgType::KeyEvent, buf));
}

#[test]
fn a_key_reaches_the_core_socket_without_allocating() {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    let conn = conn_to(ours);
    let keys = keys();
    let mut buf = Vec::new();
    for (ev, mods) in &keys {
        key_event(&conn, &mut buf, ev, *mods);
    }
    const ROUNDS: usize = 8;
    let (tally, ()) = counted(|| {
        for _ in 0..ROUNDS {
            for (ev, mods) in &keys {
                key_event(&conn, &mut buf, ev, *mods);
            }
        }
    });

    // the control: the frame as the shell built it before, a payload
    // Vec and a Frame per key, is seen by the same meter
    let (before, ()) = counted(|| {
        let mut sink = Vec::with_capacity(4096);
        let wire = marspot::shell_proto::event_to_wire(&keys[0].0, keys[0].1);
        let mut payload = Vec::new();
        marspot::shell_proto::encode_key_event(&wire, 3, &mut payload);
        Frame::new(MsgType::KeyEvent, payload.clone())
            .write_to(&mut sink)
            .unwrap();
    });
    assert!(
        before.allocs >= 1,
        "the meter did not see the old shape's payload copy"
    );

    // every key arrived, whole and in order, as itself
    drop(conn);
    let mut reader = FrameReader::new();
    let mut got = Vec::new();
    while let Some(ev) = reader
        .read_known(&mut theirs, |t, p| {
            assert_eq!(t, MsgType::KeyEvent);
            let (w, win) = decode_key_event(p).unwrap();
            assert_eq!(win, 3);
            wire_to_event(w)
        })
        .unwrap()
    {
        got.push(ev);
    }
    assert_eq!(got.len(), keys.len() * (ROUNDS + 1));
    for (i, (ev, mods)) in got.iter().enumerate() {
        let (want, want_mods) = &keys[i % keys.len()];
        assert_eq!(
            (&ev.text, ev.logical.clone(), ev.repeat, *mods),
            (&want.text, want.logical.clone(), want.repeat, *want_mods)
        );
    }
    eprintln!(
        "[key-alloc] L1 key_event -> frame written: {} allocations over {} keys",
        tally.allocs,
        ROUNDS * keys.len()
    );
    assert_eq!(
        tally.allocs,
        0,
        "the L1 key path allocated; the first:\n{}",
        tally.first.unwrap_or_default()
    );
}

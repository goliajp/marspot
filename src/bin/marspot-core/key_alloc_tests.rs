//! A key frame from L1, read, decoded, handed to the main loop and
//! dispatched to the focused session, allocates nothing once warm.

use super::*;
use crate::shadows::tests::{app_with_parent, drive, sandbox, wait_for};
use crate::shell_reader::decode_frame;
use marspot::input::{KeyState, KeyText, LogicalKey, NamedKey};
use marspot::shell_proto::{FrameReader, encode_event, write_frame};
use marspot_term::alloc_count::counted;

fn keys() -> Vec<(MarspotKeyEvent, Modifiers)> {
    let press = |logical, text: Option<&str>| MarspotKeyEvent {
        state: KeyState::Pressed,
        logical,
        text: text.map(KeyText::from),
        ..Default::default()
    };
    let release = |c: char| MarspotKeyEvent {
        state: KeyState::Released,
        logical: LogicalKey::Char(c),
        text: Some(c.into()),
        ..Default::default()
    };
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    let none = Modifiers::default();
    vec![
        (press(LogicalKey::Char('x'), Some("x")), none),
        (release('x'), none),
        (press(LogicalKey::Char('y'), Some("Y")), shift),
        (press(LogicalKey::Char('z'), Some("z")), none),
        (press(LogicalKey::Other, Some("日本語")), none),
        (press(LogicalKey::Named(NamedKey::ArrowLeft), None), none),
        (press(LogicalKey::Named(NamedKey::ArrowRight), None), none),
        (press(LogicalKey::Named(NamedKey::Enter), None), none),
    ]
}

/// An IME commit past the inline capacity: delivered whole, its text
/// on the heap once it is decoded, and nowhere else.
fn long_commit() -> (MarspotKeyEvent, Modifiers) {
    let text = "这是一段通过输入法一次提交的长句子";
    let ev = MarspotKeyEvent {
        state: KeyState::Pressed,
        logical: LogicalKey::Other,
        text: Some(text.into()),
        ..Default::default()
    };
    (ev, Modifiers::default())
}

fn screen(app: &CoreApp) -> String {
    app.focused_shown(0)
        .map(|p| {
            p.screen_rows()
                .into_iter()
                .map(|r| r.text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap()
}

#[test]
fn a_key_from_the_shell_reaches_the_session_writer_without_allocating() {
    let sb = sandbox();
    let (mut app, rx, _sid) = app_with_parent(&sb);
    let window = app.windows[0].window_id;
    assert!(
        app.focused_shown(0).is_some_and(Pane::is_l3),
        "the window shows the live session"
    );

    // the shell's end and the core's end of the control socket
    let (mut shell, mut core) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut keys = keys();
    // the long commit rides along in the warm-up and is measured apart
    keys.insert(4, long_commit());
    let mut payload = Vec::new();
    let mut type_all = || {
        for (ev, mods) in &keys {
            payload.clear();
            encode_event(ev, *mods, window, &mut payload);
            write_frame(&mut shell, MsgType::KeyEvent, &payload).unwrap();
        }
    };
    let (loop_tx, loop_rx) = marspot_term::mailbox::channel();
    let mut reader = FrameReader::new();
    // what `reader_loop` and the main loop do with each frame
    let mut one_key = |app: &mut CoreApp| {
        let ev = reader
            .read_known(&mut core, decode_frame)
            .unwrap()
            .flatten()
            .expect("a key event");
        loop_tx.send(ev).unwrap();
        match loop_rx.recv().unwrap() {
            CoreEvent::Key(event, mods, win) => app.key_from_shell(win, event, mods),
            _ => panic!("not a key"),
        }
    };

    for _ in 0..2 {
        type_all();
        for _ in 0..keys.len() {
            one_key(&mut app);
        }
    }
    const ROUNDS: usize = 4;
    for _ in 0..ROUNDS {
        type_all();
    }
    let (mut tally, mut long) = (Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
        for i in 0..keys.len() {
            let (t, ()) = counted(|| one_key(&mut app));
            if i == 4 {
                long.push(t.allocs)
            } else {
                tally.push(t)
            }
        }
    }
    let allocs: usize = tally.iter().map(|t| t.allocs).sum();
    let first = tally.into_iter().find_map(|t| t.first);

    // the control: the frame as the reader took it before, a payload
    // Vec per frame, is seen by the same meter
    type_all();
    let (before, _) = counted(|| Frame::read_from(&mut core).unwrap());
    assert!(
        before.allocs >= 1,
        "the meter did not see the old reader's payload Vec"
    );

    // what was typed reached the session: its echo is on the screen
    wait_for("the typed keys echo on the session's screen", || {
        drive(&mut app, &rx);
        app.pump_all();
        let s = screen(&app);
        s.contains("xYz这是一段通过输入法一次提交的长句子日本語")
    });
    eprintln!(
        "[key-alloc] L2 frame read -> bytes queued for L3: {allocs} allocations over {} keys; \
         a long IME commit: {long:?}",
        ROUNDS * (keys.len() - 1)
    );
    assert_eq!(
        allocs,
        0,
        "the L2 key path allocated; the first:\n{}",
        first.unwrap_or_default()
    );
    assert_eq!(
        long, [1; ROUNDS],
        "a long commit costs its decoded text and nothing else"
    );
}

//! A key is put down to the pane keys went to, across windows.

use std::time::Duration;

use marspot::shell_proto::{MsgType, encode_keyboard_pane};

use super::{hub_at, temp};

fn pause() {
    // the key clock counts milliseconds: keep each step apart
    std::thread::sleep(Duration::from_millis(20));
}

/// Two windows: pane 4 is focused in window 1, pane 5 in window 2.
#[test]
fn keys_typed_in_another_window_are_that_windows_panes() {
    let (hub, _rx) = hub_at(&temp("keyboard"));
    assert!(!hub.take_core_frame(MsgType::HelloAck, &[]));
    assert!(hub.take_core_frame(MsgType::KeyboardPane, &encode_keyboard_pane(Some(4))));
    pause();
    hub.key_pressed();
    pause();
    assert!(hub.last_input(4).is_some(), "the key went to pane 4");
    assert_eq!(hub.last_input(5), None);

    // window 2 becomes key: no window's focused pane changed, so the
    // core sends no PaneFocused, only where keys go now
    assert!(hub.take_core_frame(MsgType::KeyboardPane, &encode_keyboard_pane(Some(5))));
    assert_eq!(hub.last_input(5), None, "a key before window 2 was key is not its pane's");
    // settled: the key is kept as pane 4's from here on
    let in_4 = hub.last_input(4);
    pause();
    hub.key_pressed();
    pause();
    assert!(hub.last_input(5).is_some(), "typed in window 2");
    assert_eq!(hub.last_input(4), in_4, "pane 4 keeps only its own key");

    // window 1's focused pane moves while window 2 stays key: keys
    // still go to pane 5
    let before = hub.last_input(5);
    hub.focused(6);
    pause();
    hub.key_pressed();
    pause();
    assert_eq!(hub.last_input(6), None);
    assert!(hub.last_input(5) > before, "the key is pane 5's");

    // the app goes to the background: keys go to no pane
    assert!(hub.take_core_frame(MsgType::KeyboardPane, &encode_keyboard_pane(None)));
    let in_5 = hub.last_input(5);
    pause();
    hub.key_pressed();
    assert_eq!(hub.last_input(5), in_5);
}

/// A core that says nothing of the keyboard is followed by its
/// `PaneFocused`; a new core starts from no pane until it says.
#[test]
fn an_older_core_is_followed_by_pane_focused_and_a_new_core_starts_from_none() {
    let (hub, _rx) = hub_at(&temp("keyboard-old"));
    hub.focused(4);
    pause();
    hub.key_pressed();
    pause();
    assert!(hub.last_input(4).is_some());
    assert!(hub.take_core_frame(MsgType::KeyboardPane, &encode_keyboard_pane(Some(5))));
    hub.take_core_frame(MsgType::HelloAck, &[]);
    pause();
    hub.key_pressed();
    pause();
    assert_eq!(hub.last_input(5), None, "the new core has not said where keys go");
    hub.focused(5);
    pause();
    hub.key_pressed();
    assert!(hub.last_input(5).is_some(), "until it does, PaneFocused is followed again");
}

/// A file dropped on a pane, or the mouse reported to its program, is
/// that pane's input, wherever the keyboard is.
#[test]
fn input_the_core_reports_is_the_panes_it_names() {
    let (hub, _rx) = hub_at(&temp("keyboard-drop"));
    assert!(hub.take_core_frame(MsgType::KeyboardPane, &encode_keyboard_pane(Some(4))));
    assert!(hub.take_core_frame(MsgType::PaneUserInput, &marspot::shell_proto::encode_pane_user_input(7)));
    assert!(hub.last_input(7).is_some());
    assert_eq!(hub.last_input(4), None);
}

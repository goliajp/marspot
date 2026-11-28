//! The frames about one window by its id: it closed, it is key, close
//! it, bring it to the front.  Each payload is `window_id u32 LE`.

use std::io;

fn window_id(payload: &[u8], what: &str) -> io::Result<u32> {
    match payload.get(0..4) {
        Some(b) => Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        None => Err(io::Error::new(io::ErrorKind::InvalidData, format!("{what} payload < 4 bytes"))),
    }
}

/// A window closed.  The core drops that `WindowState` — and with it
/// the panes it held, which are closed the same way the sidebar's
/// `[x]` closes one.
pub fn encode_window_closed(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_closed(payload: &[u8]) -> io::Result<u32> {
    window_id(payload, "WINDOW_CLOSED")
}

/// Which window became key.  Keyboard and IME follow this rather than
/// tagging every keystroke: they are by definition delivered to the
/// key window, and the frames share one ordered socket.
pub fn encode_window_focus(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_focus(payload: &[u8]) -> io::Result<u32> {
    window_id(payload, "WINDOW_FOCUS")
}

pub fn encode_window_close_request(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_close_request(payload: &[u8]) -> io::Result<u32> {
    window_id(payload, "WINDOW_CLOSE_REQUEST")
}

/// The core asks for a window to come to the front: a pane of it was
/// picked in another window's sidebar.  L1 owns windows, so it asks;
/// the `WindowFocus` that follows is the answer.  An older shell skips
/// the frame and the window stays where it is.
pub fn encode_window_raise(window_id: u32) -> Vec<u8> {
    window_id.to_le_bytes().to_vec()
}

pub fn decode_window_raise(payload: &[u8]) -> io::Result<u32> {
    window_id(payload, "WINDOW_RAISE")
}

#[cfg(test)]
#[test]
fn window_frames_round_trip_and_a_short_one_is_refused() {
    assert_eq!(decode_window_raise(&encode_window_raise(7)).unwrap(), 7);
    assert_eq!(decode_window_closed(&encode_window_closed(3)).unwrap(), 3);
    assert_eq!(decode_window_focus(&encode_window_focus(u32::MAX)).unwrap(), u32::MAX);
    assert_eq!(decode_window_close_request(&encode_window_close_request(9)).unwrap(), 9);
    let err = decode_window_raise(&[1, 2, 3]).unwrap_err();
    assert!(err.to_string().contains("WINDOW_RAISE"), "{err}");
}

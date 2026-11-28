//! The frames about one window by its id: it closed, it is key, close
//! it, bring it to the front, it was minimized or restored, its own
//! controls moved.  Each payload starts with `window_id u32 LE`.

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

/// `(window_id, lights_right_phys)` — see [`super::MsgType::WindowChrome`].
pub fn encode_window_chrome(window_id: u32, lights_right_phys: f64) -> Vec<u8> {
    let mut v = window_id.to_le_bytes().to_vec();
    v.extend_from_slice(&lights_right_phys.to_le_bytes());
    v
}

pub fn decode_window_chrome(payload: &[u8]) -> io::Result<(u32, f64)> {
    let id = window_id(payload, "WINDOW_CHROME")?;
    match payload.get(4..12) {
        Some(b) => Ok((id, f64::from_le_bytes(b.try_into().expect("8 bytes")))),
        None => Err(io::Error::new(io::ErrorKind::InvalidData, "WINDOW_CHROME payload < 12 bytes")),
    }
}

/// A window was minimized to the Dock (`true`) or came back (`false`).
/// The panes it shows are then on no screen the user can see.  The
/// shell sends it on each change and, for a minimized window, again to
/// a core that takes over.  An older core skips the frame.
pub fn encode_window_minimized(window_id: u32, minimized: bool) -> Vec<u8> {
    let mut v = window_id.to_le_bytes().to_vec();
    v.push(minimized as u8);
    v
}

pub fn decode_window_minimized(payload: &[u8]) -> io::Result<(u32, bool)> {
    let id = window_id(payload, "WINDOW_MINIMIZED")?;
    match payload.get(4) {
        Some(b) => Ok((id, *b != 0)),
        None => Err(io::Error::new(io::ErrorKind::InvalidData, "WINDOW_MINIMIZED payload < 5 bytes")),
    }
}

#[cfg(test)]
#[test]
fn window_frames_round_trip_and_a_short_one_is_refused() {
    assert_eq!(decode_window_raise(&encode_window_raise(7)).unwrap(), 7);
    assert_eq!(decode_window_closed(&encode_window_closed(3)).unwrap(), 3);
    assert_eq!(decode_window_focus(&encode_window_focus(u32::MAX)).unwrap(), u32::MAX);
    assert_eq!(decode_window_close_request(&encode_window_close_request(9)).unwrap(), 9);
    assert_eq!(decode_window_minimized(&encode_window_minimized(5, true)).unwrap(), (5, true));
    assert_eq!(decode_window_minimized(&encode_window_minimized(5, false)).unwrap(), (5, false));
    assert!(decode_window_minimized(&encode_window_minimized(5, true)[..4]).is_err());
    assert_eq!(super::MsgType::from_u32(151), Some(super::MsgType::WindowMinimized));
    let err = decode_window_raise(&[1, 2, 3]).unwrap_err();
    assert!(err.to_string().contains("WINDOW_RAISE"), "{err}");
}

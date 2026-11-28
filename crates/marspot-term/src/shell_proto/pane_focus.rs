//! Which pane the user's attention and keyboard are in, core → shell.
//! Each payload is `session_id u64 LE`.

use std::io;

fn session_id(payload: &[u8], what: &str) -> io::Result<u64> {
    match <[u8; 8]>::try_from(payload) {
        Ok(b) => Ok(u64::from_le_bytes(b)),
        Err(_) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("{what} payload != 8 bytes"))),
    }
}

/// The focused pane of one window changed (`PaneFocused`).
pub fn encode_pane_focused(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_pane_focused(payload: &[u8]) -> io::Result<u64> {
    session_id(payload, "PANE_FOCUSED")
}

/// The pane keys now go to (`KeyboardPane`): the key window's focused
/// pane while the app is active, `None` while keys go to no pane.  Sent
/// on every change, and first by every core that starts.
pub fn encode_keyboard_pane(sid: Option<u64>) -> Vec<u8> {
    sid.unwrap_or(0).to_le_bytes().to_vec()
}

pub fn decode_keyboard_pane(payload: &[u8]) -> io::Result<Option<u64>> {
    session_id(payload, "KEYBOARD_PANE").map(|s| (s != 0).then_some(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_pane_round_trips_and_none_is_zero() {
        assert_eq!(decode_keyboard_pane(&encode_keyboard_pane(Some(42))).unwrap(), Some(42));
        assert_eq!(decode_keyboard_pane(&encode_keyboard_pane(None)).unwrap(), None);
        assert!(decode_keyboard_pane(&[1, 2, 3]).is_err());
        assert_eq!(decode_pane_focused(&encode_pane_focused(7)).unwrap(), 7);
    }
}

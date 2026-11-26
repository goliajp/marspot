//! `ModelConfirmed`: the person at the keyboard confirmed a `/model`
//! in Claude Code's input box of this pane, and it was sent.

use std::io;

/// `session_id u64 LE`.  Bytes after it are a later build's additions.
pub fn encode_model_confirmed(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_model_confirmed(payload: &[u8]) -> io::Result<u64> {
    payload
        .first_chunk::<8>()
        .map(|b| u64::from_le_bytes(*b))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "model_confirmed payload < 8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirmation_names_its_pane_and_a_short_one_is_refused() {
        assert_eq!(decode_model_confirmed(&encode_model_confirmed(77)).unwrap(), 77);
        assert!(decode_model_confirmed(&[1, 2, 3]).is_err());
    }
}

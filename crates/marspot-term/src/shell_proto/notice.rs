//! `Notice`: a sentence the shell wants the person at the window to
//! read, which the core shows over the focused pane for a while.

use std::io;

/// The longest notice carried; a longer one is cut at a character
/// boundary.
pub const NOTICE_MAX_BYTES: usize = 1024;

/// `text_len u16 LE, text utf8`.  Bytes after the text are a later
/// build's additions.
pub fn encode_notice(text: &str) -> Vec<u8> {
    let mut end = text.len().min(NOTICE_MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut v = Vec::with_capacity(2 + end);
    v.extend_from_slice(&(end as u16).to_le_bytes());
    v.extend_from_slice(&text.as_bytes()[..end]);
    v
}

pub fn decode_notice(payload: &[u8]) -> io::Result<String> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("notice payload: {why}"));
    let len = payload.first_chunk::<2>().map(|b| u16::from_le_bytes(*b) as usize).ok_or_else(|| bad("no length"))?;
    let text = payload.get(2..2 + len).ok_or_else(|| bad("shorter than its length"))?;
    String::from_utf8(text.to_vec()).map_err(|_| bad("not utf-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_round_trips_and_a_long_one_is_cut_on_a_character() {
        assert_eq!(decode_notice(&encode_notice("shared settings changed")).unwrap(), "shared settings changed");
        let long = "設".repeat(400);
        let back = decode_notice(&encode_notice(&long)).unwrap();
        assert!(back.len() <= NOTICE_MAX_BYTES && long.starts_with(&back) && !back.is_empty());
        assert!(decode_notice(&[5, 0, b'a']).is_err());
        assert!(decode_notice(&[]).is_err());
        let mut later = encode_notice("x");
        later.push(9);
        assert_eq!(decode_notice(&later).unwrap(), "x", "a later build's bytes are skipped");
    }
}

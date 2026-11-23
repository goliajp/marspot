//! Standard base64 (RFC 4648, with padding), for `bytes_b64`.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// None for anything that is not padded base64; whitespace is not
/// accepted inside it.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let b = text.trim().as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for (ci, chunk) in b.chunks(4).enumerate() {
        let last = ci == b.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk[..4 - pad].iter().enumerate() {
            let v = ALPHABET.iter().position(|a| a == c)? as u32;
            n |= v << (18 - 6 * i);
        }
        for i in 0..3 - pad {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod pane_control_tests {
    use super::*;

    #[test]
    fn pane_control_base64_round_trips() {
        for s in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", b"\x00\xff\x1b[A"] {
            assert_eq!(decode(&encode(s)).as_deref(), Some(s));
        }
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(decode("Zm8"), None);
        assert_eq!(decode("Z=8="), None);
        assert_eq!(decode("Zm*="), None);
    }
}

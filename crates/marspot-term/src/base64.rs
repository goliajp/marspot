//! Just enough base64 to read an OSC 52 payload.
//!
//! A program that wants the clipboard sends its text base64-encoded,
//! so reading OSC 52 means decoding it.  The whole of what is needed
//! is one direction, one alphabet, and a size cap — which is less
//! code than the line that would add a dependency.

/// Decode standard base64 (`A–Z a–z 0–9 + /`, `=` padding).
///
/// Returns `None` on any byte outside the alphabet, on a length that
/// cannot be a base64 string, or when the result would exceed `cap`.
/// Whitespace is skipped: a long payload arrives wrapped in some
/// terminals' output and the newlines are not part of the data.
pub fn decode(input: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut quad = [0u8; 4];
    let mut n = 0usize;
    let mut padding = 0usize;

    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                0
            }
            _ => return None,
        };
        // Padding is only padding at the end; a `=` with data behind
        // it is a malformed payload, not a shorter one.
        if padding > 0 && b != b'=' {
            return None;
        }
        quad[n] = v;
        n += 1;
        if n == 4 {
            let triple = ((quad[0] as u32) << 18)
                | ((quad[1] as u32) << 12)
                | ((quad[2] as u32) << 6)
                | quad[3] as u32;
            let take = 3 - padding.min(2);
            for i in 0..take {
                out.push((triple >> (16 - 8 * i)) as u8);
            }
            if out.len() > cap {
                return None;
            }
            n = 0;
        }
    }
    // A base64 string is a whole number of quads; a trailing partial
    // one means the payload was cut off.
    if n != 0 { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn it_decodes_what_a_program_would_send() {
        assert_eq!(decode(b"aGVsbG8=", 64).unwrap(), b"hello");
        assert_eq!(decode(b"aGVsbG8h", 64).unwrap(), b"hello!");
        assert_eq!(decode(b"aA==", 64).unwrap(), b"h");
        assert_eq!(decode(b"", 64).unwrap(), b"");
        // Wrapped output is still the same bytes.
        assert_eq!(decode(b"aGVs\nbG8=", 64).unwrap(), b"hello");
    }

    #[test]
    fn it_refuses_what_is_not_base64() {
        assert!(decode(b"aGVsbG8", 64).is_none(), "a partial quad is a cut-off payload");
        assert!(decode(b"aGV$bG8=", 64).is_none(), "$ is not in the alphabet");
        assert!(decode(b"aA==aA==", 64).is_none(), "padding is only padding at the end");
    }

    /// The cap is the point: a program does not get to hand the
    /// terminal a megabyte by saying it is a clipboard.
    #[test]
    fn it_refuses_more_than_the_cap() {
        let big = "A".repeat(4 * 1000);
        assert!(decode(big.as_bytes(), 64).is_none());
        assert!(decode(big.as_bytes(), 1 << 20).is_some(), "and allows what fits");
    }
}

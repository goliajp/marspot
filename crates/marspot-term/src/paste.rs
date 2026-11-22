//! Bracketed paste (DECSET 2004) framing shared by every path that
//! writes pasted text into a PTY.

const START: &[u8; 6] = b"\x1b[200~";
const END: &[u8; 6] = b"\x1b[201~";

/// Append `text` to `out` wrapped in `ESC[200~` ... `ESC[201~`, with
/// every start and end marker removed from the text first.
///
/// An end marker left in the text would close the paste early and hand
/// the rest to the program as typed keys.  A start marker is dropped
/// too: it carries no content, and stripping only one of the two would
/// let `ESC[20` + `ESC[200~` + `1~` rebuild the other.  Removal runs on
/// the bytes already written, so a marker that only appears once an
/// inner one is gone (`ESC[20ESC[201~1~`) is removed as well.
pub fn bracketed_paste(text: &[u8], out: &mut Vec<u8>) {
    out.reserve(text.len() + START.len() + END.len());
    out.extend_from_slice(START);
    let body = out.len();
    if !text.contains(&0x1b) {
        out.extend_from_slice(text);
    } else {
        for &b in text {
            out.push(b);
            if b == b'~' && out.len() - body >= END.len() {
                let tail = &out[out.len() - END.len()..];
                if tail == END || tail == START {
                    out.truncate(out.len() - END.len());
                }
            }
        }
    }
    out.extend_from_slice(END);
}

#[cfg(test)]
mod tests {
    use super::bracketed_paste;

    fn paste(text: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        bracketed_paste(text, &mut out);
        out
    }

    #[test]
    fn paste_plain_text_is_wrapped() {
        assert_eq!(paste(b"hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(b""), b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn paste_drops_end_and_start_markers_in_the_text() {
        assert_eq!(paste(b"a\x1b[201~rm -rf ~\n"), b"\x1b[200~arm -rf ~\n\x1b[201~");
        assert_eq!(paste(b"\x1b[200~x\x1b[201~"), b"\x1b[200~x\x1b[201~");
        assert_eq!(paste(b"\x1b[201~\x1b[201~"), b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn paste_markers_cannot_be_rebuilt_by_nesting() {
        assert_eq!(paste(b"\x1b[20\x1b[201~1~"), b"\x1b[200~\x1b[201~");
        assert_eq!(paste(b"\x1b[20\x1b[200~1~z"), b"\x1b[200~z\x1b[201~");
        assert_eq!(paste(b"\x1b[20\x1b[20\x1b[201~1~1~"), b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn paste_keeps_other_escapes() {
        assert_eq!(paste(b"\x1b[31mred\x1b[202~"), b"\x1b[200~\x1b[31mred\x1b[202~\x1b[201~");
    }

    #[test]
    fn paste_appends_to_what_is_already_there() {
        // bytes written before the paste are left alone
        let mut out = b"\x1b[201".to_vec();
        bracketed_paste(b"~x", &mut out);
        assert_eq!(out, b"\x1b[201\x1b[200~~x\x1b[201~");
    }
}

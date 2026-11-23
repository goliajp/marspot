//! The key names `pane.send` takes in `keys`, and the bytes each
//! writes.  A fixed table: anything else goes as `bytes_b64`.

const TABLE: &[(&str, &[u8])] = &[
    ("enter", b"\r"),
    ("return", b"\r"),
    ("tab", b"\t"),
    ("backtab", b"\x1b[Z"),
    ("esc", b"\x1b"),
    ("escape", b"\x1b"),
    ("space", b" "),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
    ("pageup", b"\x1b[5~"),
    ("pagedown", b"\x1b[6~"),
    ("f1", b"\x1bOP"),
    ("f2", b"\x1bOQ"),
    ("f3", b"\x1bOR"),
    ("f4", b"\x1bOS"),
    ("f5", b"\x1b[15~"),
    ("f6", b"\x1b[17~"),
    ("f7", b"\x1b[18~"),
    ("f8", b"\x1b[19~"),
    ("f9", b"\x1b[20~"),
    ("f10", b"\x1b[21~"),
    ("f11", b"\x1b[23~"),
    ("f12", b"\x1b[24~"),
];

/// The bytes of key `name`: one from the table, or `ctrl-<letter>`.
pub fn bytes(name: &str) -> Option<&'static [u8]> {
    if let Some((_, b)) = TABLE.iter().find(|(n, _)| *n == name) {
        return Some(b);
    }
    let letter = name.strip_prefix("ctrl-")?;
    let [c] = letter.as_bytes() else { return None };
    CTRL.get(c.to_ascii_lowercase().checked_sub(b'a')? as usize).map(std::slice::from_ref)
}

const CTRL: [u8; 26] = {
    let mut t = [0u8; 26];
    let mut i = 0;
    while i < 26 {
        t[i] = i as u8 + 1;
        i += 1;
    }
    t
};

/// Every accepted name, for an error message.
pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&str> = TABLE.iter().map(|(n, _)| *n).collect();
    v.push("ctrl-a..ctrl-z");
    v
}

#[cfg(test)]
mod pane_control_tests {
    use super::*;

    #[test]
    fn pane_control_key_names_map_to_bytes() {
        assert_eq!(bytes("ctrl-c"), Some(&[3u8][..]));
        assert_eq!(bytes("ctrl-a"), Some(&[1u8][..]));
        assert_eq!(bytes("ctrl-z"), Some(&[26u8][..]));
        assert_eq!(bytes("enter"), Some(&b"\r"[..]));
        assert_eq!(bytes("up"), Some(&b"\x1b[A"[..]));
        assert_eq!(bytes("ctrl-1"), None);
        assert_eq!(bytes("ctrl-"), None);
        assert_eq!(bytes("hyper"), None);
    }
}

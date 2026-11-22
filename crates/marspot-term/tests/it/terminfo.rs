//! marspot sets `TERM=xterm-256color` and ships no entry of its own,
//! so every capability that entry declares has to be true here.  The
//! entry is committed as `tests/data/xterm-256color.terminfo` (the
//! output of `infocmp -1`), and every capability in it maps to a check:
//! a string is fed to the terminal, or to the key encoder for a key,
//! and the resulting state compared with what the capability claims.
//! A capability with no check fails the test, so a newer entry cannot
//! slip in a claim nobody looked at.

use marspot_term::terminal::Terminal;

#[path = "terminfo_keys.rs"]
mod keys;
#[path = "terminfo_screen.rs"]
mod screen;
#[path = "terminfo_screen_more.rs"]
mod screen_more;

/// What the entry declares that marspot does not do, and why.  Each
/// one's check asserts what marspot does instead.  Keep this list to
/// what cannot be made true.
pub const EXCEPTIONS: &[(&str, &str)] = &[
    // every macOS terminal sends DEL for Backspace and shells expect it
    ("kbs", "Backspace sends DEL (0x7f), not ^H"),
    // a Mac keypad has no NumLock, so no key is KP_Begin
    ("kb2", "no key on a Mac keyboard is the keypad's Begin"),
    // oh-my-zsh sends smkx at every prompt and zsh binds nothing to
    // `ESC O M`, so the entry's string would break Enter at the prompt
    ("kent", "the keypad's Enter sends Return, not ESC O M"),
    // there is no printer: the media copy sequences are accepted and
    // ignored, and what a program meant for the printer stays on screen
    ("mc0", "printing the screen does nothing"),
    ("mc4", "there is no printer to turn off"),
    ("mc5", "printer controller mode is ignored; the output stays on screen"),
    ("mc5i", "what is meant for the printer is shown on screen"),
];

/// Checked where the encoder lives: the pointer encoder is the session
/// process's, so `kmous` is checked by its tests (`terminfo_kmous`).
const ELSEWHERE: &[&str] = &["kmous"];

pub enum Cap {
    Bool,
    Num(i64),
    /// The decoded bytes, `%` parameters still in, padding taken out.
    Str(Vec<u8>),
}

pub fn entry() -> Vec<(String, Cap)> {
    let text = include_str!("../data/xterm-256color.terminfo");
    let mut lines = text.lines().filter(|l| !l.starts_with('#'));
    let name = lines.next().expect("a name line");
    assert!(name.starts_with("xterm-256color|"), "{name}");
    lines
        .map(|l| {
            let l = l.trim_start().strip_suffix(',').expect("a capability ends with a comma");
            if let Some((k, v)) = l.split_once('=') {
                (k.to_string(), Cap::Str(decode(v)))
            } else if let Some((k, v)) = l.split_once('#') {
                (k.to_string(), Cap::Num(v.parse().expect("a number")))
            } else {
                (l.to_string(), Cap::Bool)
            }
        })
        .collect()
}

/// The escapes `infocmp` writes, back to bytes; `$<…>` padding is a
/// delay for the program's `tputs`, never sent, so it goes.
fn decode(v: &str) -> Vec<u8> {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                i += 1;
                match b[i] {
                    b'E' | b'e' => out.push(0x1b),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b's' => out.push(b' '),
                    b'0'..=b'7' => {
                        let n = u8::from_str_radix(&v[i..i + 3], 8).expect("octal");
                        out.push(if n == 0o200 { 0 } else { n });
                        i += 2;
                    }
                    c => out.push(c),
                }
                i += 1;
            }
            b'^' => {
                out.push(if b[i + 1] == b'?' { 0x7f } else { b[i + 1] & 0x1f });
                i += 2;
            }
            b'$' if b.get(i + 1) == Some(&b'<') => {
                i += b[i..].iter().position(|&c| c == b'>').expect("padding ends") + 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `tparm`: the `%` language of a parameterised string.  A `%d` with
/// nothing pushed takes the next parameter, as the report formats
/// (`u6`) are written.
pub fn tparm(s: &[u8], params: &[i64]) -> Vec<u8> {
    let mut p = [0i64; 9];
    p[..params.len()].copy_from_slice(params);
    let mut st: Vec<i64> = Vec::new();
    let mut next = 0;
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c != b'%' {
            out.push(c);
            continue;
        }
        let op = s[i];
        i += 1;
        match op {
            b'%' => out.push(b'%'),
            b'i' => {
                p[0] += 1;
                p[1] += 1;
            }
            b'p' => {
                st.push(p[(s[i] - b'1') as usize]);
                i += 1;
            }
            b'{' => {
                let end = i + s[i..].iter().position(|&c| c == b'}').unwrap();
                st.push(std::str::from_utf8(&s[i..end]).unwrap().parse().unwrap());
                i = end + 1;
            }
            b'd' => out.extend(pop(&mut st, &p, &mut next).to_string().bytes()),
            b'c' => out.push(pop(&mut st, &p, &mut next) as u8),
            b'0'..=b'9' | b'.' => {
                // only the form the entry uses: `%2.2X`
                let end = i + s[i..].iter().position(|c| c.is_ascii_alphabetic()).unwrap();
                assert_eq!(&s[i - 1..=end], b"2.2X", "format {:?}", String::from_utf8_lossy(&s[i - 1..=end]));
                out.extend(format!("{:02X}", pop(&mut st, &p, &mut next)).bytes());
                i = end + 1;
            }
            b'+' | b'-' | b'*' | b'/' | b'm' | b'|' | b'&' | b'<' | b'>' | b'=' => {
                let b = pop(&mut st, &p, &mut next);
                let a = pop(&mut st, &p, &mut next);
                st.push(match op {
                    b'+' => a + b,
                    b'-' => a - b,
                    b'*' => a * b,
                    b'/' => a / b,
                    b'm' => a % b,
                    b'|' => a | b,
                    b'&' => a & b,
                    b'<' => (a < b) as i64,
                    b'>' => (a > b) as i64,
                    _ => (a == b) as i64,
                });
            }
            b'?' | b';' => {}
            b't' => {
                if pop(&mut st, &p, &mut next) == 0 {
                    i = skip(s, i, true);
                }
            }
            b'e' => i = skip(s, i, false),
            _ => panic!("tparm: %{}", op as char),
        }
    }
    out
}

fn pop(st: &mut Vec<i64>, p: &[i64; 9], next: &mut usize) -> i64 {
    st.pop().unwrap_or_else(|| {
        *next += 1;
        p[*next - 1]
    })
}

/// From just after a `%t` whose condition failed (`to_else`) or a `%e`
/// whose branch is done: past the matching `%e` or `%;`.
fn skip(s: &[u8], mut i: usize, to_else: bool) -> usize {
    let mut depth = 0;
    while i + 1 < s.len() {
        if s[i] == b'%' {
            match s[i + 1] {
                b'?' => depth += 1,
                b';' if depth == 0 => return i + 2,
                b';' => depth -= 1,
                b'e' if depth == 0 && to_else => return i + 2,
                _ => {}
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    s.len()
}

pub fn term() -> Terminal {
    Terminal::new(80, 24)
}

/// Check one capability; false when there is no check for it.
fn check(name: &str, cap: &Cap) -> bool {
    match cap {
        Cap::Bool => screen::boolean(name),
        Cap::Num(n) => screen::number(name, *n),
        Cap::Str(s) if name.starts_with('k') => keys::key(name, s),
        Cap::Str(s) => screen::string(name, s),
    }
}

#[test]
fn terminfo_every_capability_of_xterm_256color_holds() {
    let entry = entry();
    assert!(entry.len() > 150, "the fixture parsed to {} capabilities", entry.len());
    let mut unchecked = Vec::new();
    for (name, cap) in &entry {
        if ELSEWHERE.contains(&name.as_str()) {
            continue;
        }
        if !check(name, cap) {
            unchecked.push(name.as_str());
        }
    }
    assert!(unchecked.is_empty(), "capabilities with no check: {unchecked:?}");
}

#[test]
fn terminfo_exceptions_are_capabilities_of_the_entry() {
    let entry = entry();
    for (name, _) in EXCEPTIONS.iter().map(|(n, r)| (*n, *r)) {
        assert!(entry.iter().any(|(k, _)| k == name), "{name} is not in the entry");
    }
    for name in ELSEWHERE {
        assert!(entry.iter().any(|(k, _)| k == name), "{name} is not in the entry");
    }
    assert_eq!(EXCEPTIONS.len(), 7, "the exception list only shrinks");
}

#[test]
fn terminfo_tparm_reads_the_entry_like_ncurses() {
    assert_eq!(tparm(b"\x1b[%i%p1%d;%p2%dH", &[4, 9]), b"\x1b[5;10H");
    assert_eq!(tparm(b"\x1b[%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;m", &[3]), b"\x1b[33m");
    assert_eq!(tparm(b"\x1b[%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;m", &[12]), b"\x1b[94m");
    assert_eq!(tparm(b"\x1b[%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;m", &[200]), b"\x1b[38;5;200m");
    assert_eq!(tparm(b"%p1%{255}%*%{1000}%/%2.2X", &[1000]), b"FF");
    assert_eq!(tparm(b"\x1b[%i%d;%dR", &[2, 7]), b"\x1b[3;8R");
    assert_eq!(decode("\\E[?5h$<100/>\\E[?5l"), b"\x1b[?5h\x1b[?5l");
    assert_eq!(decode("^G\\:\\,"), b"\x07:,");
}

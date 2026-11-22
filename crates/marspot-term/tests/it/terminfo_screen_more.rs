//! The cursor, editing, mode and report strings.

use super::screen::{at, fed, first_letters, lettered, row};
use super::{tparm, EXCEPTIONS};

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// A cursor at row 10, column 20 (0-based), then `s` with `params`.
fn from_middle(s: &[u8], params: &[i64]) -> (u16, u16) {
    let t = fed(&[b"\x1b[11;21H".to_vec(), tparm(s, params)].concat());
    at(&t)
}

pub fn string(name: &str, s: &[u8]) -> bool {
    match name {
        // cursor motion: (column, row) after the move
        "cub" => assert_eq!(from_middle(s, &[3]), (17, 10)),
        "cub1" => assert_eq!(from_middle(s, &[]), (19, 10)),
        "cuf" => assert_eq!(from_middle(s, &[3]), (23, 10)),
        "cuf1" => assert_eq!(from_middle(s, &[]), (21, 10)),
        "cuu" => assert_eq!(from_middle(s, &[3]), (20, 7)),
        "cuu1" => assert_eq!(from_middle(s, &[]), (20, 9)),
        "cud" => assert_eq!(from_middle(s, &[3]), (20, 13)),
        // a line feed: down a row, column kept
        "cud1" | "ind" => assert_eq!(from_middle(s, &[]), (20, 11)),
        "cr" => assert_eq!(from_middle(s, &[]), (0, 10)),
        "home" => assert_eq!(from_middle(s, &[]), (0, 0)),
        "cup" => assert_eq!(from_middle(s, &[4, 9]), (9, 4)),
        "hpa" => assert_eq!(from_middle(s, &[5]), (5, 10)),
        "vpa" => assert_eq!(from_middle(s, &[7]), (20, 7)),
        "ht" => assert_eq!(from_middle(s, &[]), (24, 10)),
        "cbt" => assert_eq!(from_middle(s, &[]), (16, 10)),
        "hts" => {
            let t = fed(&cat(&[b"\x1b[4G".as_slice(), s, b"\r\t"]));
            assert_eq!(at(&t), (3, 0));
        }
        "tbc" => {
            let t = fed(&cat(&[s, b"\t"]));
            assert_eq!(at(&t), (79, 0));
        }
        "sc" | "rc" => {
            let t = fed(b"\x1b[4;5H\x1b7\x1b[H\x1b8");
            assert_eq!(at(&t), (4, 3));
            assert!(s == b"\x1b7" || s == b"\x1b8");
        }
        // scrolling: the first letter of every row after it
        "csr" => {
            let mut t = lettered();
            t.feed(&tparm(s, &[2, 5]));
            assert_eq!(at(&t), (0, 0), "setting the region homes the cursor");
            t.feed(b"\x1b[6;1H\n");
            assert_eq!(first_letters(&t), "ABDEF GHIJKLMNOPQRSTUVWX");
        }
        "indn" | "rin" | "il" | "dl" | "il1" | "dl1" | "ri" => {
            let mut t = lettered();
            let start: &[u8] = if name == "ri" { b"" } else { b"\x1b[3;1H" };
            t.feed(&[start.to_vec(), tparm(s, &[2])].concat());
            let want = match name {
                "indn" => "CDEFGHIJKLMNOPQRSTUVWX  ",
                "rin" => "  ABCDEFGHIJKLMNOPQRSTUV",
                "il" => "AB  CDEFGHIJKLMNOPQRSTUV",
                "dl" => "ABEFGHIJKLMNOPQRSTUVWX  ",
                "il1" => "AB CDEFGHIJKLMNOPQRSTUVW",
                "dl1" => "ABDEFGHIJKLMNOPQRSTUVWX ",
                _ => " ABCDEFGHIJKLMNOPQRSTUVW",
            };
            assert_eq!(first_letters(&t), want, "{name}");
        }
        // editing a line `abcdef` with the cursor on `b`
        "dch" | "dch1" | "ich" | "ech" | "el" | "el1" => {
            let t = fed(&[b"abcdef\x1b[2G".to_vec(), tparm(s, &[2])].concat());
            let want = match name {
                "dch" => "adef",
                "dch1" => "acdef",
                "ich" => "a  bcdef",
                "ech" => "a  def",
                "el" => "a",
                _ => "  cdef",
            };
            assert_eq!(row(&t, 0), want, "{name}");
        }
        "ed" | "clear" => {
            let mut t = lettered();
            t.feed(b"\x1b[3;1H");
            t.feed(s);
            let want = if name == "ed" { format!("AB{}", " ".repeat(22)) } else { " ".repeat(24) };
            assert_eq!(first_letters(&t), want, "{name}");
        }
        "smcup" | "rmcup" => {
            let mut t = fed(b"main");
            t.feed(s);
            if name == "smcup" {
                assert!(t.in_alt_screen() && row(&t, 0).is_empty());
            } else {
                assert!(!t.in_alt_screen());
                t.feed(b"\x1b[?1049halt");
                t.feed(s);
                assert_eq!((t.in_alt_screen(), row(&t, 0).as_str()), (false, "main"));
            }
        }
        "smir" | "rmir" => {
            let t = fed(&cat(&[b"abc\x1b[4h\x1b[H".as_slice(), s, b"X"]));
            assert_eq!(row(&t, 0), if name == "smir" { "Xabc" } else { "Xbc" });
        }
        "smam" | "rmam" => {
            let t = fed(&cat(&[s, &[b'x'; 81]]));
            let (r0, r1) = (row(&t, 0).len(), row(&t, 1).len());
            assert_eq!((r0, r1), if name == "smam" { (80, 1) } else { (80, 0) }, "{name}");
            assert_eq!(t.screen_modes().autowrap, name == "smam");
        }
        "smkx" | "rmkx" => {
            let t = fed(&cat(&[b"\x1b[?1h\x1b=".as_slice(), s]));
            let on = name == "smkx";
            assert_eq!((t.input_modes().cursor_key_app, t.screen_modes().keypad_app), (on, on), "{name}");
        }
        "smm" | "rmm" => {
            let t = fed(&cat(&[b"\x1b[?1034h".as_slice(), s]));
            assert_eq!(t.screen_modes().meta_mode, name == "smm");
        }
        "civis" | "cnorm" | "cvvis" => {
            let t = fed(&cat(&[b"\x1b[?25l\x1b[?12h".as_slice(), s]));
            let (visible, blink) = (t.cursor_visible(), t.screen_modes().cursor_blink);
            let want = match name {
                "civis" => (false, true),
                "cnorm" => (true, false),
                _ => (true, true),
            };
            assert_eq!((visible, blink), want, "{name}");
        }
        // a soft reset as `tput init` and `reset` send it
        "is2" | "rs2" => {
            let mut t = fed(b"\x1b[4h\x1b=\x1b[1;31m\x1b[5;10rtext");
            t.feed(s);
            assert!(row(&t, 0).is_empty() && at(&t) == (0, 0));
            assert_eq!(t.current_attrs(), Default::default());
            assert!(!t.screen_modes().keypad_app);
            t.feed(b"ab\rX");
            assert_eq!(row(&t, 0), "Xb", "insert mode is off");
            t.feed(b"\x1b[24;1HQ\n");
            assert_eq!(row(&t, 22), "Q", "the scroll region is the whole screen");
        }
        "rs1" => {
            let mut t = fed(b"\x1b[?1049h\x1b[1mtext\x1b]4;1;rgb:ff/00/00\x07");
            t.feed(s);
            assert!(!t.in_alt_screen() && row(&t, 0).is_empty() && at(&t) == (0, 0));
            assert_eq!(t.current_attrs(), Default::default());
            assert!(t.screen_modes().palette.is_empty());
        }
        // printing: there is no printer, so every media copy sequence
        // is accepted and ignored, and the output stays on screen
        "mc0" | "mc4" => {
            assert!(EXCEPTIONS.iter().any(|(n, _)| *n == name));
            let t = fed(&cat(&[b"ab".as_slice(), s, b"c"]));
            assert_eq!(row(&t, 0), "abc");
        }
        "mc5" => {
            assert!(EXCEPTIONS.iter().any(|(n, _)| *n == name));
            // nothing waits for `CSI 4 i`: the bytes after are drawn and
            // the sequences in them act, a reset included
            let t = fed(&cat(&[s, b"shown \x1b[1mbold"]));
            assert_eq!((row(&t, 0).as_str(), t.current_attrs().bold), ("shown bold", true));
            let mut t = fed(&cat(&[s, b"x\x1bc"]));
            t.feed(b"after reset");
            assert_eq!(row(&t, 0), "after reset");
        }
        // memory lock: rows above the cursor stop scrolling
        "meml" | "memu" => {
            let mut t = lettered();
            t.feed(b"\x1b[3;1H\x1bl");
            if name == "memu" {
                t.feed(s);
            } else {
                assert_eq!(s, b"\x1bl");
            }
            t.feed(b"\x1b[24;1H\n");
            let want = if name == "meml" { "ABDEFGHIJKLMNOPQRSTUVWX " } else { "BCDEFGHIJKLMNOPQRSTUVWX " };
            assert_eq!(first_letters(&t), want, "{name}");
        }
        // cursor position report: `u7` asks, the answer has `u6`'s shape
        "u7" => {
            let mut t = fed(b"\x1b[6;12H");
            t.feed(s);
            assert_eq!(t.take_response(), tparm(b"\x1b[%i%d;%dR", &[5, 11]));
        }
        "u6" => assert_eq!(tparm(s, &[5, 11]), b"\x1b[6;12R"),
        // primary device attributes: `u9` asks, `u8` is the answer
        "u9" => {
            let mut t = fed(s);
            assert_eq!(t.take_response(), b"\x1b[?1;2c");
        }
        "u8" => {
            let mut t = fed(b"\x1b[c");
            assert_eq!(t.take_response(), s);
        }
        _ => return false,
    }
    true
}

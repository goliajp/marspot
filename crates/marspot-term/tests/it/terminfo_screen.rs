//! The boolean, numeric and screen-string capabilities: each string is
//! fed to a fresh 80x24 terminal and the grid, the cursor, the modes,
//! the bytes written back or the published frame are compared with
//! what the capability claims.

use marspot_term::grid::{Cell, Color};
use marspot_term::grid_shm::cells::VERSION_PACKED;
use marspot_term::grid_shm::{GridShmReader, GridShmWriter};
use marspot_term::terminal::Terminal;

use super::{term, tparm, EXCEPTIONS};

pub fn fed(bytes: &[u8]) -> Terminal {
    let mut t = term();
    t.feed(bytes);
    t
}

pub fn row(t: &Terminal, r: u16) -> String {
    let s: String = (0..t.grid().cols()).map(|c| t.grid().cell(c, r).ch).collect();
    s.trim_end().to_string()
}

pub fn at(t: &Terminal) -> (u16, u16) {
    t.grid().cursor()
}

/// Row `r` holds the letter `A + r`, cursor home.
pub fn lettered() -> Terminal {
    let mut t = term();
    for r in 0..24u8 {
        t.feed(format!("\x1b[{};1H{}", r + 1, (b'A' + r) as char).as_bytes());
    }
    t.feed(b"\x1b[H");
    t
}

pub fn first_letters(t: &Terminal) -> String {
    (0..24).map(|r| t.grid().cell(0, r).ch).collect()
}

/// The frame a session publishes for this terminal, read back the way
/// the renderer reads it, in either cell layout.
pub fn frame(t: &Terminal, packed: bool) -> Vec<Cell> {
    let g = t.grid();
    let mut w = GridShmWriter::create(g.cols(), g.rows()).unwrap();
    if packed {
        w.set_reader_frames(VERSION_PACKED);
    }
    let r = GridShmReader::from_fd(unsafe { libc::dup(w.fd()) }).unwrap();
    w.publish_screen_modes(t.screen_modes());
    w.publish(g, 0, 0);
    let mut cells = Vec::new();
    r.read(&mut cells, &mut Vec::new()).expect("a frame");
    cells
}

pub fn boolean(name: &str) -> bool {
    match name {
        // a glyph past the last column wraps to the next row
        "am" => {
            let t = fed(&[b'x'; 81]);
            assert_eq!((row(&t, 0).len(), row(&t, 1).as_str()), (80, "x"));
        }
        // erasing fills with the background colour in force
        "bce" => {
            let t = fed(b"\x1b[41m\x1b[K\x1b[2J");
            assert_eq!(t.grid().cell(79, 0).attrs.bg, Color::indexed(1));
            assert_eq!(t.grid().cell(40, 23).attrs.bg, Color::indexed(1));
        }
        // palette entries can be changed: `initc` is checked below
        "ccc" => {
            let t = fed(b"\x1b]4;3;rgb:12/34/56\x1b\\");
            assert_eq!(t.screen_modes().palette.get(3), Some((0x12, 0x34, 0x56)));
        }
        "km" => super::keys::meta_key(),
        // there is no printer: what a program meant for it is shown
        "mc5i" => {
            assert!(EXCEPTIONS.iter().any(|(n, _)| *n == name));
            let t = fed(b"\x1b[5ishown\x1b[4i");
            assert_eq!((row(&t, 0).as_str(), at(&t)), ("shown", (5, 0)));
        }
        // moving the cursor in insert mode moves it and inserts nothing
        "mir" => {
            let t = fed(b"abc\x1b[4h\x1b[H\x1b[C\x1b[DX");
            assert_eq!(row(&t, 0), "Xabc");
        }
        // moving the cursor with attributes set paints nothing
        "msgr" => {
            let t = fed(b"\x1b[7m\x1b[5;5H\x1b[3D\x1b[2B\x1b[H\r\n");
            assert!((0..80).all(|c| (0..24).all(|r| !t.grid().cell(c, r).attrs.reverse)));
        }
        // no padding character is needed: NUL is not drawn
        "npc" => {
            let t = fed(b"a\0\0b");
            assert_eq!((row(&t, 0).as_str(), at(&t)), ("ab", (2, 0)));
        }
        // the cursor stays on the last column, and a newline after it
        // does not make an extra blank line
        "xenl" => {
            let mut t = fed(&[b'x'; 80]);
            assert_eq!(at(&t), (79, 0));
            t.feed(b"\r\nz");
            assert_eq!((row(&t, 1).as_str(), at(&t)), ("z", (1, 1)));
        }
        _ => return false,
    }
    true
}

pub fn number(name: &str, n: i64) -> bool {
    match name {
        "colors" => {
            assert_eq!(n, 256);
            let t = fed(b"\x1b[38;5;255m\x1b[48;5;16mx");
            assert_eq!(t.grid().cell(0, 0).attrs.fg, Color::indexed(255));
            assert_eq!(t.grid().cell(0, 0).attrs.bg, Color::indexed(16));
        }
        // any foreground with any background
        "pairs" => {
            assert_eq!(n, 32767);
            let t = fed(b"\x1b[38;5;201;48;5;202mx\x1b[38;5;202;48;5;201my");
            let (a, b) = (t.grid().cell(0, 0).attrs, t.grid().cell(1, 0).attrs);
            assert_eq!((a.fg, a.bg, b.fg, b.bg), (Color::indexed(201), Color::indexed(202), Color::indexed(202), Color::indexed(201)));
        }
        // the size of a fresh terminal; a program reads the real one
        // from the tty, and asking the terminal gives the same
        "cols" | "lines" => {
            let mut t = term();
            assert_eq!((t.grid().cols() as i64, t.grid().rows() as i64), (80, 24));
            assert_eq!(if name == "cols" { 80 } else { 24 }, n);
            t.feed(b"\x1b[18t");
            assert_eq!(t.take_response(), b"\x1b[8;24;80t");
        }
        // tab stops every eighth column
        "it" => {
            let t = fed(b"\tx\ty");
            assert_eq!(n, 8);
            assert_eq!((t.grid().cell(8, 0).ch, t.grid().cell(16, 0).ch), ('x', 'y'));
        }
        _ => return false,
    }
    true
}

/// The DEC Special Graphics glyph for each character `acsc` names.
const DEC: &[(u8, char)] = &[
    (b'`', '◆'), (b'a', '▒'), (b'f', '°'), (b'g', '±'), (b'i', '␋'), (b'j', '┘'), (b'k', '┐'),
    (b'l', '┌'), (b'm', '└'), (b'n', '┼'), (b'o', '⎺'), (b'p', '⎻'), (b'q', '─'), (b'r', '⎼'),
    (b's', '⎽'), (b't', '├'), (b'u', '┤'), (b'v', '┴'), (b'w', '┬'), (b'x', '│'), (b'y', '≤'),
    (b'z', '≥'), (b'{', 'π'), (b'|', '≠'), (b'}', '£'), (b'~', '·'),
];

pub fn string(name: &str, s: &[u8]) -> bool {
    if super::screen_more::string(name, s) {
        return true;
    }
    match name {
        // every pair maps a VT100 graphic to itself, drawn as that glyph
        "acsc" => {
            for pair in s.chunks(2) {
                assert_eq!(pair[0], pair[1]);
                let want = DEC.iter().find(|(c, _)| *c == pair[0]).expect("a DEC graphic").1;
                let t = fed(&[b"\x1b(0".as_slice(), &pair[1..]].concat());
                assert_eq!(t.grid().cell(0, 0).ch, want, "acsc {}", pair[0] as char);
            }
        }
        "smacs" | "rmacs" => {
            let t = fed(&[b"\x1b(0q".as_slice(), s, b"q".as_slice()].concat());
            let want = if name == "smacs" { '─' } else { 'q' };
            assert_eq!((t.grid().cell(0, 0).ch, t.grid().cell(1, 0).ch), ('─', want));
        }
        "bel" => {
            assert!(EXCEPTIONS.iter().any(|(n, _)| *n == name));
            let mut t = fed(b"ab");
            t.feed(s);
            assert_eq!((row(&t, 0).as_str(), at(&t)), ("ab", (2, 0)));
            assert!(t.take_response().is_empty());
        }
        "blink" | "bold" | "dim" | "rev" | "smso" | "smul" | "sitm" | "invis" => {
            let t = fed(&[s, b"x".as_slice()].concat());
            let a = t.grid().cell(0, 0).attrs;
            let on = match name {
                "blink" => a.blink(),
                "bold" => a.bold,
                "dim" => a.dim,
                "rev" | "smso" => a.reverse,
                "smul" => a.underline,
                "sitm" => a.italic,
                _ => a.invisible(),
            };
            assert!(on, "{name}");
            for packed in [false, true] {
                let f = frame(&t, packed)[0];
                match name {
                    // blinking text is drawn steady
                    "blink" => assert!(!f.attrs.blink()),
                    // concealed text goes out blank; the grid keeps it to copy
                    "invis" => assert_eq!((f.ch, t.grid().cell(0, 0).ch), (' ', 'x')),
                    _ => assert_eq!(f.ch, 'x'),
                }
            }
        }
        "rmso" | "rmul" | "ritm" | "op" | "sgr0" => {
            let t = fed(&[b"\x1b[1;3;4;7;31;42m\x1b(0".as_slice(), s, b"q".as_slice()].concat());
            let a = t.grid().cell(0, 0).attrs;
            match name {
                "rmso" => assert!(!a.reverse && a.bold),
                "rmul" => assert!(!a.underline && a.bold),
                "ritm" => assert!(!a.italic && a.bold),
                "op" => assert!(a.fg == Color::DEFAULT && a.bg == Color::DEFAULT && a.bold),
                _ => assert_eq!((a, t.grid().cell(0, 0).ch), (Default::default(), 'q')),
            }
        }
        "setaf" | "setab" => {
            for i in 0..=255 {
                let t = fed(&[tparm(s, &[i]), b"x".to_vec()].concat());
                let a = t.grid().cell(0, 0).attrs;
                assert_eq!(if name == "setaf" { a.fg } else { a.bg }, Color::indexed(i as u8), "{name} {i}");
            }
        }
        // p1 standout, p2 underline, p3 reverse, p4 blink, p5 dim,
        // p6 bold, p7 invisible, p8 protected (not in the string), p9
        // the line-drawing set
        "sgr" => {
            for k in 0..9 {
                let mut p = [0i64; 9];
                p[k] = 1;
                let t = fed(&[b"\x1b[3m".to_vec(), tparm(s, &p), b"q".to_vec()].concat());
                let c = t.grid().cell(0, 0);
                let a = c.attrs;
                let got = [a.reverse, a.underline, a.reverse, a.blink(), a.dim, a.bold, a.invisible(), false, c.ch == '─'];
                let want: Vec<bool> = (0..9).map(|j| k != 7 && (j == k || (k == 0 && j == 2) || (k == 2 && j == 0))).collect();
                assert_eq!(got.to_vec(), want, "sgr p{}", k + 1);
                assert!(!a.italic, "sgr starts from normal");
            }
        }
        "flash" => {
            // the delay between the two halves is the program's
            let half = s.windows(5).position(|w| w == b"\x1b[?5l").unwrap();
            let mut t = fed(&[b"x".as_slice(), &s[..half]].concat());
            assert!(t.screen_modes().reverse_screen);
            for packed in [false, true] {
                assert!(frame(&t, packed)[0].attrs.reverse, "the screen shows reversed");
            }
            t.feed(&s[half..]);
            assert!(!t.screen_modes().reverse_screen);
            assert!(!frame(&t, true)[0].attrs.reverse);
        }
        "initc" => {
            let mut t = fed(&[tparm(s, &[1, 1000, 0, 500]), b"\x1b[31mx".to_vec()].concat());
            assert_eq!(t.screen_modes().palette.get(1), Some((255, 0, 127)));
            for packed in [false, true] {
                assert_eq!(frame(&t, packed)[0].attrs.fg, Color::rgb(255, 0, 127));
            }
            assert_eq!(t.grid().cell(0, 0).attrs.fg, Color::indexed(1));
            t.feed(b"\x1b]4;1;?\x07");
            assert_eq!(t.take_response(), b"\x1b]4;1;rgb:ffff/0000/7f7f\x07");
            t.feed(b"\x1b]104\x07");
            assert_eq!(frame(&t, true)[0].attrs.fg, Color::indexed(1));
        }
        _ => return false,
    }
    true
}

//! Whether a batch of output can have put a text watch's text on the
//! grid, read off the bytes as they are taken in, so that the grid is
//! read only for a batch that can have.
//!
//! Text the program prints left to right reaches the grid as it is in
//! the stream, with colour sequences between its bytes and soft wraps
//! that do not show in it.  A batch says "check" when the stream holds
//! a whole match, and also whenever the cursor was moved in a way that
//! can line up new cells with old ones (cursor addressing, a lone
//! carriage return, a lone line feed, backspace, tab, a charset
//! switch), when the grid is checked whatever the stream says.
//!
//! A chunk that holds none of the texts' last bytes cannot complete a
//! match and is read only from its last line feed on, when that line is
//! short.  Otherwise what state it left is unknown, and the next chunk
//! that is read says "check" if one of the last bytes comes before its
//! first line break.
//!
//! Which texts the stream held whole is said apart from that, for a
//! check on rows the batch went to and left again: a cursor that moved
//! is no sign that the text is new there.  Text drawn on the alternate
//! screen is never one of them.

#[path = "prefilter/plain.rs"]
mod plain;
#[path = "prefilter/screen.rs"]
mod screen;
#[path = "prefilter/text.rs"]
mod text;

use screen::Private;
use text::Text;

/// Texts looked for at once: one per text watch.
const TEXTS: usize = 8;
/// Longest last line of a chunk without a text's last byte that is read.
const TAIL_READ: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Esc,
    /// `ESC` and intermediate bytes: a charset or line-size switch.
    EscInter,
    /// Just after `CSI`: a `?` starts a private mode sequence.
    CsiFirst,
    Csi,
    Private,
    /// OSC: until BEL or `ESC \`.
    Osc,
    /// DCS, SOS, PM, APC: until `ESC \`, as the parser takes no BEL
    /// for their end.
    Ignore,
}

pub(crate) struct Prefilter {
    texts: Vec<Text>,
    /// Bytes a run of plain text stops at: controls, and each text's
    /// first byte.
    stops: Box<[bool; 256]>,
    /// The texts' first bytes, each repeated over a word.
    firsts: Vec<u64>,
    /// Each text's last byte, for the look before a chunk is read.
    lasts: [u8; TEXTS],
    mode: Mode,
    cr: bool,
    /// The state the last chunk left is not known.
    unknown: bool,
    check: bool,
    /// The texts the bytes held whole, by id.
    seen: u8,
    /// Bytes were left unread and no line break came since, so a text
    /// may have begun in them; and the text bytes read since.
    skipped: bool,
    after_skip: Vec<u8>,
    /// The bytes draw on the alternate screen; set to the grid's at
    /// each ask.
    alt: bool,
    private: Private,
}

impl Default for Prefilter {
    fn default() -> Self {
        Self {
            texts: Vec::new(),
            stops: Box::new([false; 256]),
            firsts: Vec::new(),
            lasts: [0; TEXTS],
            mode: Mode::Text,
            cr: false,
            unknown: true,
            check: false,
            seen: 0,
            skipped: false,
            after_skip: Vec::new(),
            alt: false,
            private: Private::default(),
        }
    }
}

impl Prefilter {
    /// Look for `texts` from now on; none stops looking.
    pub(crate) fn set(&mut self, texts: &[(usize, &[u8])]) {
        let same = |t: &&Text| texts.iter().any(|&(id, bytes)| t.id == id && t.bytes == bytes);
        let kept = self.texts.iter().filter(same).fold(0, |k, t| k | 1 << t.id);
        self.seen &= kept;
        self.texts = texts.iter().take(TEXTS).map(|&(id, t)| Text::new(id, t)).collect();
        self.after_skip.clear();
        self.after_skip.reserve(self.longest());
        let mut stops = [false; 256];
        stops[..0x20].fill(true);
        for (i, t) in self.texts.iter().enumerate() {
            stops[usize::from(t.bytes[0])] = true;
            self.lasts[i] = t.last();
        }
        *self.stops = stops;
        self.firsts = self.texts.iter().map(|t| plain::word(t.bytes[0])).collect();
        self.firsts.dedup();
        // what is owed for output a held grid has not parsed stays owed
        self.forget();
    }

    /// The bytes from now on draw on the alternate screen: for the
    /// first text set, as the bytes are not followed while none is.
    pub(crate) fn starts_on(&mut self, alt: bool) {
        self.alt = alt;
    }

    pub(crate) fn active(&self) -> bool {
        !self.texts.is_empty()
    }

    fn longest(&self) -> usize {
        self.texts.iter().map(|t| t.bytes.len()).max().unwrap_or(0)
    }

    /// Whether a batch since the last ask has to be checked on the
    /// grid, and a bit for the id of each text it held whole.  `alt`
    /// says the grid is on the alternate screen: the bits of text drawn
    /// before it was entered are kept until it is left.
    pub(crate) fn take(&mut self, alt: bool) -> (bool, u8) {
        self.alt = alt;
        let seen = if alt { 0 } else { std::mem::take(&mut self.seen) };
        (std::mem::take(&mut self.check), seen)
    }

    pub(crate) fn took(&mut self, bytes: &[u8]) {
        if self.texts.is_empty() {
            return;
        }
        let lasts = &self.lasts[..self.texts.len()];
        if lasts.iter().any(|&b| super::has_byte(bytes, b)) {
            self.read(bytes);
            return;
        }
        // no match ends here, but one can start in the last line: that
        // line is read when it is short, so a chunk cut before a line's
        // last byte leaves a known state
        // only as far back as a line that is read can start
        let near = bytes.len().saturating_sub(TAIL_READ + 1);
        let tail = bytes[near..].iter().rposition(|&b| b == b'\n').map_or(near, |at| near + at + 1);
        if bytes.len() - tail > TAIL_READ {
            // an escape the chunk ends in is read, so that the rest of
            // it in the next chunk is not taken for text
            if let Some(at) = self.skip(bytes).filter(|&at| bytes.len() - at <= TAIL_READ) {
                self.read(&bytes[at..]);
            }
            return;
        }
        if tail > 0 {
            self.skip(&bytes[..tail]);
            // a line break inside a string sequence the skip left open
            // moves nothing
            if self.mode == Mode::Text {
                self.line_break();
            }
        }
        self.read(&bytes[tail..]);
    }

    /// Leave `bytes` unread; where their last escape is.  A string
    /// sequence they leave open goes on, as its payload can be longer
    /// than any line that is read.
    fn skip(&mut self, bytes: &[u8]) -> Option<usize> {
        let before = self.mode;
        self.forget();
        let (switch, last) = screen::scan(bytes);
        if let Some(on) = switch {
            self.alt = on;
        }
        let (open, rest) = match last {
            Some(at) => (esc_string(bytes.get(at + 1)), bytes.get(at + 2..).unwrap_or_default()),
            None => (before, bytes),
        };
        let ends: &[u8] = match open {
            Mode::Osc => &[0x07, 0x18, 0x1a],
            Mode::Ignore => &[0x18, 0x1a],
            _ => return last,
        };
        if !ends.iter().any(|&b| super::has_byte(rest, b)) {
            self.mode = open;
        }
        last
    }

    fn forget(&mut self) {
        self.unknown = true;
        self.mode = Mode::Text;
        self.cr = false;
        self.skipped = true;
        self.after_skip.clear();
        for t in &mut self.texts {
            t.state = 0;
        }
    }

    /// The cursor went where new cells can line up with old ones.
    fn moved(&mut self) {
        self.check = true;
        // what the bytes say next no longer follows the grid, up to a line break
        self.unknown = true;
        self.skipped = false;
    }

    fn line_break(&mut self) {
        self.unknown = false;
        self.skipped = false;
        for t in &mut self.texts {
            t.state = 0;
        }
    }

    fn read(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            let after_cr = std::mem::take(&mut self.cr);
            if after_cr && b != b'\n' {
                self.moved();
            }
            match self.mode {
                Mode::Text => {
                    self.text_byte(b, after_cr);
                    if self.mode == Mode::Text && self.idle() {
                        i = plain::end(&self.firsts, &self.stops, bytes, i);
                    }
                }
                // as in the parser: an escape starts anew in any sequence,
                // CAN and SUB end it
                _ if is_abort(b) => self.abort(b),
                Mode::Esc => self.esc_byte(b),
                Mode::EscInter => {
                    if (0x30..=0x7e).contains(&b) {
                        self.mode = Mode::Text;
                    }
                }
                Mode::CsiFirst if b == b'?' => {
                    self.mode = Mode::Private;
                    self.private = Private::default();
                }
                Mode::CsiFirst | Mode::Csi => {
                    self.mode = Mode::Csi;
                    // parameters and intermediates up to the final byte
                    let mut b = b;
                    while (0x20..0x40).contains(&b) && i < bytes.len() {
                        b = bytes[i];
                        i += 1;
                    }
                    if (0x40..=0x7e).contains(&b) {
                        self.csi_end(b);
                    } else if is_abort(b) {
                        self.abort(b);
                    }
                }
                Mode::Private => self.private_byte(b),
                Mode::Osc => {
                    if b == 0x07 {
                        self.mode = Mode::Text;
                    }
                }
                Mode::Ignore => {}
            }
        }
    }

    fn csi_end(&mut self, b: u8) {
        self.mode = Mode::Text;
        // colour, erasing and reports leave cells where they are;
        // mode changes can move the cursor (origin mode, leaving
        // the alternate screen), so they count as moves
        if !b"mKJXnctqp".contains(&b) {
            self.moved();
        }
    }

    fn abort(&mut self, b: u8) {
        self.mode = if b == 0x1b { Mode::Esc } else { Mode::Text };
    }

    fn private_byte(&mut self, b: u8) {
        if !(0x40..=0x7e).contains(&b) {
            self.private.byte(b);
            return;
        }
        if let Some(on) = self.private.end(b) {
            self.alt = on;
        }
        self.csi_end(b);
    }

    fn idle(&self) -> bool {
        !self.unknown && !self.cr && self.texts.iter().all(|t| t.state == 0)
    }

    fn text_byte(&mut self, b: u8, after_cr: bool) {
        match b {
            0x1b => self.mode = Mode::Esc,
            b'\r' => self.cr = true,
            b'\n' if after_cr => self.line_break(),
            // a lone line feed keeps the column: the row's old cells stay beside it
            b'\n' | 0x08 | 0x09 | 0x0b | 0x0c | 0x0e | 0x0f => self.moved(),
            0..=0x1f | 0x7f => {}
            _ => {
                if self.skipped {
                    self.after_skip.push(b);
                    self.skipped = self.after_skip.len() < self.longest();
                }
                for t in &mut self.texts {
                    let last = t.last() == b;
                    let whole = t.step(b) || (self.skipped && last && t.bytes.ends_with(&self.after_skip));
                    self.seen |= u8::from(whole && !self.alt) << t.id;
                    self.check |= whole || (self.unknown && last);
                }
            }
        }
    }

    fn esc_byte(&mut self, b: u8) {
        self.mode = match b {
            b'[' => Mode::CsiFirst,
            b']' | b'P' | b'X' | b'^' | b'_' => esc_string(Some(&b)),
            b'=' | b'>' | b'\\' => Mode::Text,
            0x20..=0x2f => {
                self.moved();
                Mode::EscInter
            }
            _ => {
                // a full reset leaves the alternate screen
                self.alt &= b != b'c';
                self.moved();
                Mode::Text
            }
        };
    }
}

/// Whether output that leaves a grid on `alt`, then `chunks` not yet
/// parsed into it, draws on the alternate screen.
pub(super) fn screen_after(alt: bool, chunks: &[Vec<u8>]) -> bool {
    chunks.iter().rev().find_map(|c| screen::last_switch(c)).unwrap_or(alt)
}

/// The string sequence the byte after an escape starts, if any.
fn esc_string(b: Option<&u8>) -> Mode {
    match b {
        Some(b']') => Mode::Osc,
        Some(b'P' | b'X' | b'^' | b'_') => Mode::Ignore,
        _ => Mode::Text,
    }
}

/// ESC, CAN and SUB, which end any sequence being read.
fn is_abort(b: u8) -> bool {
    matches!(b, 0x1b | 0x18 | 0x1a)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bring `p` to a known state: a chunk it reads, with a line break
    /// before the last byte.
    fn settle(p: &mut Prefilter) {
        p.took(b"\r\nE\r\n");
        assert!(!p.take(false).0);
    }

    fn set(text: &str) -> Prefilter {
        let mut p = Prefilter::default();
        p.set(&[(0, text.as_bytes())]);
        settle(&mut p);
        p
    }

    fn after(p: &mut Prefilter, chunks: &[&str]) -> bool {
        for c in chunks {
            p.took(c.as_bytes());
        }
        p.take(false).0
    }

    #[test]
    fn watch_prefilter_says_check_for_a_whole_match_only() {
        let mut p = set("DONE");
        assert!(!after(&mut p, &["xDON\r\nE\r\n"]), "a line break between");
        assert!(after(&mut p, &["all DONE\r\n"]));
        settle(&mut p);
        assert!(after(&mut p, &["E\r\nD\x1b[1mO\x1b[0mN", "E"]), "colour between the bytes, and a chunk boundary");
        settle(&mut p);
        assert!(!after(&mut p, &["E\x1b]0;DONE\x07\r\n"]), "a title is not on the grid");
        assert!(after(&mut p, &["E\r\nx\x1b]0;title\x07DONE"]));
        settle(&mut p);
        assert!(after(&mut p, &["DDDONE"]), "a partial match goes on");
    }

    #[test]
    fn watch_prefilter_says_check_when_the_cursor_moves() {
        let moves = ["a\rbE", "a\nbE", "a\x08bE", "a\tbE", "\x1b[2;3HE", "\x1b[AE", "\x1bME", "\x1b(0qE"];
        for m in moves {
            let mut p = set("XE");
            assert!(after(&mut p, &[m]), "{m:?}");
        }
        let mut p = set("XE");
        assert!(!after(&mut p, &["\x1b[31mE\x1b[K\x1b[0m\r\n"]), "colour and erasing leave the cursor");
        assert!(after(&mut p, &["aE\r", "bE"]), "a carriage return at a chunk's end, with no line feed after");
    }

    #[test]
    fn watch_prefilter_reads_only_chunks_with_a_last_byte_and_is_careful_after_one_it_skipped() {
        let mut p = set("DONE");
        assert!(!after(&mut p, &["x\r\nDON"]), "no E: only its last line is read");
        assert!(after(&mut p, &["E\r\n"]), "the match it began ends in the next chunk");
        let long = format!("x\r\n{}DON", "y".repeat(TAIL_READ));
        assert!(!after(&mut p, &[&long]));
        assert!(after(&mut p, &["xE\r\n"]), "a long last line is not read: the state it left is unknown");
        assert!(!after(&mut p, &["xyz", "\r\nE\r\n"]), "unknown only up to the first line break");
        let mut none = Prefilter::default();
        none.set(&[]);
        assert!(!after(&mut none, &["DONE"]));
    }

    #[test]
    fn watch_prefilter_tells_a_whole_text_from_a_moved_cursor() {
        let held = |p: &mut Prefilter, chunks: &[&str]| {
            chunks.iter().for_each(|c| p.took(c.as_bytes()));
            p.take(false).1
        };
        let mut p = Prefilter::default();
        p.set(&[(2, b"DONE"), (5, b"FIVE")]);
        settle(&mut p);
        assert_eq!(held(&mut p, &["\x1b[AxE"]), 0, "only the last byte");
        assert_eq!(held(&mut p, &["\x1b[H\x1b[2JFIVE\r\n"]), 1 << 5);
        assert_eq!(held(&mut p, &["DON", "E\r\n"]), 1 << 2);
        let long = format!("x\r\n{}DO", "y".repeat(TAIL_READ));
        assert_eq!(held(&mut p, &[&long, "NE"]), 1 << 2, "begun in bytes not read");
        assert_eq!(held(&mut p, &["\r\n", &long, "xE"]), 0, "not what the text ends with");
        assert_eq!(held(&mut p, &["\r\n", &long, "\x1b[5;1HE"]), 0, "nor beside it");
        p.took(b"\r\nDONE FIVE");
        p.set(&[(2, b"DONE"), (5, b"GONE")]);
        assert_eq!(p.take(false), (true, 1 << 2), "kept for the text that stays");
    }

    #[test]
    fn watch_prefilter_holds_no_text_drawn_on_the_alternate_screen() {
        let mut p = set("DONE");
        p.took(b"\x1b[?1049hDONE\x1b[?1049l");
        assert_eq!(p.take(false), (true, 0), "drawn there and left");
        p.took(b"DONE\x1b[?1;1049h");
        assert_eq!(p.take(true), (true, 0), "kept while it is on");
        p.took(b"\x1b[?1049l");
        assert_eq!(p.take(false), (true, 1), "given once it is left");
        let unread = format!("\x1b[?1049h{}", "y".repeat(TAIL_READ + 1));
        p.took(unread.as_bytes());
        p.took(b"\r\nDONE");
        assert_eq!(p.take(false).1, 0, "entered in bytes not read");
        let cut = format!("x\r\n{}DO\x1b[", "y".repeat(TAIL_READ));
        p.took(cut.as_bytes());
        p.took(b"1mNE");
        assert_eq!(p.take(false).1, 1, "unread bytes that end in an escape");
    }

    #[test]
    fn watch_prefilter_ends_a_sequence_where_the_parser_does() {
        let held = |chunk: &str| {
            let mut p = set("DONE");
            p.took(chunk.as_bytes());
            p.take(false).1
        };
        // in order: an escape in a private mode sequence, in a sequence's
        // parameters, CAN, SUB, an escape in a title
        let chunks = [
            "\x1b[?\x1b[?1049hDONE\x1b[?1049l",
            "\x1b[1\x1b[?1049hDONE\x1b[?1049l",
            "\x1b[?1\x18049hDONE",
            "\x1b[?1\x1a049hDONE",
            "\x1b]0;title\x1b[1mDONE\r\n",
        ];
        assert_eq!(chunks.map(held), [0, 0, 1, 1, 1]);
    }

    #[test]
    fn watch_prefilter_ends_dcs_sos_pm_apc_only_where_the_parser_does() {
        let held = |chunk: &str| {
            let mut p = set("DONE");
            p.took(chunk.as_bytes());
            p.take(false).1
        };
        // BEL ends a title, not the others
        let chunks = ["\x1b]0;t\x07DONE\r\n", "\x1bPq\x07DONE\x1b\\", "\x1bX\x07DONE\x1b\\", "\x1b^\x07DONE\x1b\\", "\x1b_G\x07DONE\x1b\\"];
        assert_eq!(chunks.map(held), [1, 0, 0, 0, 0]);
    }

    #[test]
    fn watch_prefilter_keeps_a_string_open_across_bytes_not_read() {
        let payload = "A".repeat(TAIL_READ + 100);
        let held = |chunks: &[String]| {
            let mut p = set("DONE");
            chunks.iter().for_each(|c| p.took(c.as_bytes()));
            p.take(false).1
        };
        // begun before the last bytes looked at, in a chunk and the one
        // after, with a line break in it, and ended in bytes not read
        let cases = [
            vec![format!("\x1b_Ga=q;{payload}"), "DONE\x1b\\\r\n".into()],
            vec![format!("\x1b]52;c;{payload}"), payload.clone(), "DONE\x07\r\n".into()],
            vec![format!("\x1bPq{payload}\nAA"), "DONE\x1b\\\r\n".into()],
            vec![format!("\x1b_Ga=q;{payload}\x1b\\{payload}"), "\r\nDONE".into()],
            vec![format!("\x1b]0;t{payload}\x07{payload}"), "\r\nDONE".into()],
        ];
        assert_eq!(cases.map(|c| held(&c)), [0, 0, 0, 1, 1]);
    }

    #[test]
    fn watch_prefilter_does_not_take_a_line_break_in_an_open_string_for_one() {
        let mut p = set("DONE");
        p.took(b"DO\x1b]0;a\nb");
        p.took(b"\x07NE");
        assert_eq!(p.take(false).1, 1);
    }

    #[test]
    fn watch_prefilter_switches_screens_only_where_the_terminal_does() {
        let held = |unread: &[u8], read: &[u8]| {
            let mut p = set("DONE");
            p.took(unread);
            p.took(read);
            p.take(false).1
        };
        let skips = format!("{}\r\n", "y".repeat(TAIL_READ + 1));
        let sixteen = format!("\x1b[?{}1049h", ";".repeat(16));
        // the parser ignores these, or they have more than a `?`
        let ignored: [&[u8]; 5] = [b"\x1b[?1049$h", b"\x1b[?1049?h", b"\x1b[?1049\xc3h", b"\x1b[?>1049h", sixteen.as_bytes()];
        for seq in ignored {
            let read = [seq, b"DONE\r\n"].concat();
            assert_eq!(held(b"", &read), 1, "read {:?}", String::from_utf8_lossy(seq));
            let unread = [seq, skips.as_bytes()].concat();
            assert_eq!(held(&unread, b"DONE"), 1, "not read {:?}", String::from_utf8_lossy(seq));
        }
        // a colon's parameter is set too, and a control in the sequence
        // is executed with the sequence going on
        for seq in [&b"\x1b[?1049:1h"[..], b"\x1b[?1:1049h", b"\x1b[?10\x0749h", b"\x1b[?1049\rh"] {
            let read = [seq, b"DONE\r\n"].concat();
            assert_eq!(held(b"", &read), 0, "read {:?}", String::from_utf8_lossy(seq));
            let unread = [seq, skips.as_bytes()].concat();
            assert_eq!(held(&unread, b"DONE"), 0, "not read {:?}", String::from_utf8_lossy(seq));
        }
    }

    #[test]
    fn watch_prefilter_leaves_the_alternate_screen_on_a_full_reset() {
        // read, in bytes not read, and the last switch in them counting
        let mut p = set("DONE");
        let mut held = |unread: &str, read: &str| {
            p.took(unread.as_bytes());
            p.took(read.as_bytes());
            p.take(false).1
        };
        let long = "y".repeat(TAIL_READ + 1);
        let seen = [
            held("", "\x1b[?1049h\x1bcDONE"),
            held(&format!("\x1b[?1049hy\x1bc{long}"), "\r\nDONE"),
            held(&format!("\x1bc\x1b[?1049h\x1b[?25l{long}"), "\r\nDONE"),
        ];
        assert_eq!(seen, [1, 1, 0]);
    }

    /// How fast build-like output is read when every line has the
    /// text's last byte; printed, for the parse's cost in the perf test.
    #[test]
    fn watch_prefilter_reading_speed() {
        let mut out = Vec::new();
        for n in 0..200_000u64 {
            out.extend_from_slice(format!("   Compiling crate-{n:06} v0.{}.{} (\x1b[32mok\x1b[0m) in {}ms\r\n", n % 7, n % 13, n % 997).as_bytes());
        }
        let mut p = set("never printed ms");
        let started = std::time::Instant::now();
        let mut checks = 0;
        for chunk in out.chunks(64 * 1024) {
            p.took(chunk);
            checks += usize::from(p.take(false).0);
        }
        let took = started.elapsed();
        eprintln!("prefilter: {:.0} MiB/s, {checks} checks", out.len() as f64 / took.as_secs_f64() / (1 << 20) as f64);
    }
}

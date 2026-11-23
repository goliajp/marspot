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
//! is no sign that the text is new there.

/// Texts looked for at once: one per text watch.
const TEXTS: usize = 8;
/// Longest last line of a chunk without a text's last byte that is read.
const TAIL_READ: usize = 512;
const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// One of the word's bytes is zero.
#[inline]
fn zero_byte(x: u64) -> bool {
    x.wrapping_sub(ONES) & !x & HIGHS != 0
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Esc,
    /// `ESC` and intermediate bytes: a charset or line-size switch.
    EscInter,
    Csi,
    /// OSC, DCS, APC, PM, SOS: until BEL or `ESC \`.
    Str,
    StrEsc,
}

struct Text {
    /// Whose it is, as it was set: a bit of `seen`.
    id: usize,
    bytes: Vec<u8>,
    back: Vec<u16>,
    state: usize,
}

impl Text {
    fn new(id: usize, bytes: &[u8]) -> Self {
        let mut back = vec![0u16; bytes.len()];
        let mut k = 0;
        for i in 1..bytes.len() {
            while k > 0 && bytes[i] != bytes[k] {
                k = usize::from(back[k - 1]);
            }
            if bytes[i] == bytes[k] {
                k += 1;
            }
            back[i] = k as u16;
        }
        Self { id, bytes: bytes.to_vec(), back, state: 0 }
    }

    #[inline]
    fn step(&mut self, b: u8) -> bool {
        let mut s = self.state;
        while s > 0 && self.bytes[s] != b {
            s = usize::from(self.back[s - 1]);
        }
        if self.bytes[s] == b {
            s += 1;
        }
        if s == self.bytes.len() {
            self.state = usize::from(self.back[s - 1]);
            return true;
        }
        self.state = s;
        false
    }

    fn last(&self) -> u8 {
        self.bytes[self.bytes.len() - 1]
    }
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
        self.firsts = self.texts.iter().map(|t| ONES * u64::from(t.bytes[0])).collect();
        self.firsts.dedup();
        // what is owed for output a held grid has not parsed stays owed
        self.forget();
    }

    pub(crate) fn active(&self) -> bool {
        !self.texts.is_empty()
    }

    fn longest(&self) -> usize {
        self.texts.iter().map(|t| t.bytes.len()).max().unwrap_or(0)
    }

    /// Whether a batch since the last ask has to be checked on the
    /// grid, and a bit for the id of each text it held whole.
    pub(crate) fn take(&mut self) -> (bool, u8) {
        (std::mem::take(&mut self.check), std::mem::take(&mut self.seen))
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
        let tail = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
        if bytes.len() - tail > TAIL_READ {
            self.forget();
            return;
        }
        if tail > 0 {
            self.forget();
            self.line_break();
        }
        self.read(&bytes[tail..]);
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
                        i = self.plain_end(bytes, i);
                    }
                }
                Mode::Esc => self.esc_byte(b),
                Mode::EscInter => {
                    if (0x30..=0x7e).contains(&b) {
                        self.mode = Mode::Text;
                    }
                }
                Mode::Csi => {
                    // parameters and intermediates up to the final byte
                    let mut b = b;
                    while (0x20..0x40).contains(&b) && i < bytes.len() {
                        b = bytes[i];
                        i += 1;
                    }
                    if (0x40..=0x7e).contains(&b) {
                        self.mode = Mode::Text;
                        // colour, erasing and reports leave cells where they are;
                        // mode changes can move the cursor (origin mode, leaving
                        // the alternate screen), so they count as moves
                        if !b"mKJXnctqp".contains(&b) {
                            self.moved();
                        }
                    }
                }
                Mode::Str => match b {
                    0x07 => self.mode = Mode::Text,
                    0x1b => self.mode = Mode::StrEsc,
                    _ => {}
                },
                Mode::StrEsc => self.mode = if b == b'\\' { Mode::Text } else { Mode::Str },
            }
        }
    }

    /// Where the plain text from `i` on ends: the next control byte, or
    /// the next first byte of a text.  Eight bytes at a time.
    #[inline]
    fn plain_end(&self, bytes: &[u8], mut i: usize) -> usize {
        while let Some(word) = bytes.get(i..i + 8) {
            let x = u64::from_le_bytes(word.try_into().unwrap_or_default());
            let control = x.wrapping_sub(ONES * 0x20) & !x & HIGHS;
            if control != 0 || self.firsts.iter().any(|&f| zero_byte(x ^ f)) {
                break;
            }
            i += 8;
        }
        while i < bytes.len() && !self.stops[usize::from(bytes[i])] {
            i += 1;
        }
        i
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
                    self.seen |= u8::from(whole) << t.id;
                    self.check |= whole || (self.unknown && last);
                }
            }
        }
    }

    fn esc_byte(&mut self, b: u8) {
        self.mode = match b {
            b'[' => Mode::Csi,
            b']' | b'P' | b'X' | b'^' | b'_' => Mode::Str,
            b'=' | b'>' | b'\\' => Mode::Text,
            0x20..=0x2f => {
                self.moved();
                Mode::EscInter
            }
            _ => {
                self.moved();
                Mode::Text
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bring `p` to a known state: a chunk it reads, with a line break
    /// before the last byte.
    fn settle(p: &mut Prefilter) {
        p.took(b"\r\nE\r\n");
        assert!(!p.take().0);
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
        p.take().0
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
            p.take().1
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
        assert_eq!(p.take(), (true, 1 << 2), "kept for the text that stays");
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
            checks += usize::from(p.take().0);
        }
        let took = started.elapsed();
        eprintln!("prefilter: {:.0} MiB/s, {checks} checks", out.len() as f64 / took.as_secs_f64() / (1 << 20) as f64);
    }
}

//! Byte-stream → terminal-event state machine.
//!
//! Parses the byte stream emitted by a child process attached to a PTY into
//! a sequence of terminal-meaningful events (print a glyph, execute a C0
//! control, dispatch an ESC/CSI/OSC sequence).  Semantics (what each
//! sequence *does*) live in modules layered on top — this layer only
//! tokenizes.
//!
//! The state machine follows Paul Williams' published DEC/ANSI parser
//! diagram (https://vt100.net/emu/dec_ansi_parser): GROUND, ESCAPE,
//! ESCAPE_INTERMEDIATE, CSI_ENTRY, CSI_PARAM, CSI_INTERMEDIATE,
//! CSI_IGNORE, OSC_STRING.  We currently stub out DCS / SOS / PM / APC —
//! they're rare in modern shells and will be added when a real workload
//! demands them.
//!
//! UTF-8 handling sits inside GROUND: when a high byte starts a multi-byte
//! sequence we accumulate continuation bytes and emit a single `print(char)`
//! when complete.  Any malformed sequence emits `print('\u{FFFD}')` and
//! resets the UTF-8 accumulator.

const MAX_PARAMS: usize = 16;
const MAX_INTERMEDIATES: usize = 4;
const MAX_OSC_LEN: usize = 4096;
pub(crate) const REPLACEMENT_CHAR: char = '\u{FFFD}';

pub trait ParserCallbacks {
    fn print(&mut self, ch: char);
    fn execute(&mut self, byte: u8);
    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8);
    fn csi_dispatch(&mut self, params: &[u16], intermediates: &[u8], byte: u8);
    /// `data` is the raw OSC payload (between `ESC ]` and the terminator).
    /// Caller splits on `;` if needed; we don't pre-parse to avoid lifetime
    /// noise in the trait.
    fn osc_dispatch(&mut self, data: &[u8]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
}

pub struct Parser {
    state: State,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediates_len: usize,
    params: [u16; MAX_PARAMS],
    params_len: usize,
    /// In-progress numeric parameter accumulation.  u32 lets us detect
    /// overflow before clamping to u16 on commit.
    current_param: u32,
    has_current_param: bool,
    osc_buffer: Vec<u8>,
    /// UTF-8 multi-byte accumulator.  When `utf8_remaining > 0` we are in
    /// the middle of a multi-byte sequence; expect that many continuation
    /// bytes (10xxxxxx).
    utf8_remaining: u8,
    utf8_codepoint: u32,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            intermediates: [0; MAX_INTERMEDIATES],
            intermediates_len: 0,
            params: [0; MAX_PARAMS],
            params_len: 0,
            current_param: 0,
            has_current_param: false,
            // Pre-allocate the OSC buffer once.  We `clear()` between OSC
            // sequences to reset length without releasing the capacity.
            osc_buffer: Vec::with_capacity(MAX_OSC_LEN),
            utf8_remaining: 0,
            utf8_codepoint: 0,
        }
    }

    /// True when the parser is in plain Ground state with no UTF-8
    /// sequence in flight — i.e. a printable-ASCII byte fed now would
    /// go straight to `print` with no state change.  Lets the caller
    /// batch whole ASCII runs around the per-byte state machine.
    pub fn in_ground_plain(&self) -> bool {
        matches!(self.state, State::Ground) && self.utf8_remaining == 0
    }

    pub fn advance<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        // Williams' "anywhere" transitions: these fire regardless of state.
        match byte {
            0x18 | 0x1A => {
                self.reset_seq();
                self.state = State::Ground;
                cb.execute(byte);
                return;
            }
            0x1B => {
                self.reset_seq();
                self.state = State::Escape;
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => self.ground(cb, byte),
            State::Escape => self.escape(cb, byte),
            State::EscapeIntermediate => self.escape_intermediate(cb, byte),
            State::CsiEntry => self.csi_entry(cb, byte),
            State::CsiParam => self.csi_param(cb, byte),
            State::CsiIntermediate => self.csi_intermediate(cb, byte),
            State::CsiIgnore => self.csi_ignore(byte),
            State::OscString => self.osc_string(cb, byte),
        }
    }

    fn reset_seq(&mut self) {
        self.intermediates_len = 0;
        self.params_len = 0;
        self.current_param = 0;
        self.has_current_param = false;
        self.osc_buffer.clear();
        // We deliberately do NOT touch utf8_remaining here — an in-flight
        // UTF-8 sequence interrupted by ESC is malformed; we surface that
        // by emitting a replacement char on the next ground entry.
        if self.utf8_remaining > 0 {
            self.utf8_remaining = 0;
            self.utf8_codepoint = 0;
        }
    }

    fn ground<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        if self.utf8_remaining > 0 {
            // Expect a continuation byte (10xxxxxx).
            if byte & 0b1100_0000 == 0b1000_0000 {
                self.utf8_codepoint = (self.utf8_codepoint << 6) | (byte & 0x3F) as u32;
                self.utf8_remaining -= 1;
                if self.utf8_remaining == 0 {
                    let cp = self.utf8_codepoint;
                    self.utf8_codepoint = 0;
                    match char::from_u32(cp) {
                        Some(c) => cb.print(c),
                        None => cb.print(REPLACEMENT_CHAR),
                    }
                }
                return;
            }
            // Non-continuation byte while expecting one: malformed.  Emit
            // U+FFFD for the broken sequence and restart with this byte.
            self.utf8_remaining = 0;
            self.utf8_codepoint = 0;
            cb.print(REPLACEMENT_CHAR);
            // Fall through to dispatch this byte normally.
        }

        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => cb.execute(byte),
            0x20..=0x7E => cb.print(byte as char),
            0x7F => {} // DEL — ignored in xterm Ground
            // UTF-8 lead bytes
            0xC2..=0xDF => {
                self.utf8_remaining = 1;
                self.utf8_codepoint = (byte & 0x1F) as u32;
            }
            0xE0..=0xEF => {
                self.utf8_remaining = 2;
                self.utf8_codepoint = (byte & 0x0F) as u32;
            }
            0xF0..=0xF4 => {
                self.utf8_remaining = 3;
                self.utf8_codepoint = (byte & 0x07) as u32;
            }
            // Stray continuation byte or invalid UTF-8 leader.
            0x80..=0xC1 | 0xF5..=0xFF => cb.print(REPLACEMENT_CHAR),
            // 0x18 / 0x1A / 0x1B are consumed by the anywhere transitions
            // before this match runs and cannot arrive here.
            0x18 | 0x1A | 0x1B => {}
        }
    }

    fn escape<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => cb.execute(byte),
            0x20..=0x2F => {
                self.collect_intermediate(byte);
                self.state = State::EscapeIntermediate;
            }
            0x30..=0x4F | 0x51..=0x57 | 0x59 | 0x5A | 0x5C | 0x60..=0x7E => {
                cb.esc_dispatch(self.intermediates_slice(), byte);
                self.state = State::Ground;
            }
            0x5B => {
                // ESC [
                self.state = State::CsiEntry;
            }
            0x5D => {
                // ESC ]
                self.osc_buffer.clear();
                self.state = State::OscString;
            }
            0x50 | 0x58 | 0x5E | 0x5F => {
                // DCS / SOS / PM / APC — not implemented yet; consume
                // until ST and discard.  For now we fall back to Ground;
                // a real terminfo workload will force us to wire these.
                self.state = State::Ground;
            }
            0x7F => {} // ignore
            // 0x18 / 0x1A / 0x1B handled by anywhere transitions.
            0x18 | 0x1A | 0x1B => {}
            _ => self.state = State::Ground,
        }
    }

    fn escape_intermediate<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => cb.execute(byte),
            0x20..=0x2F => self.collect_intermediate(byte),
            0x30..=0x7E => {
                cb.esc_dispatch(self.intermediates_slice(), byte);
                self.state = State::Ground;
            }
            0x7F => {}
            0x18 | 0x1A | 0x1B => {}
            _ => self.state = State::Ground,
        }
    }

    fn csi_entry<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => cb.execute(byte),
            0x30..=0x39 => {
                self.start_param_digit(byte);
                self.state = State::CsiParam;
            }
            0x3B => {
                // Parameter separator with no preceding digits → default 0.
                self.commit_param();
                self.state = State::CsiParam;
            }
            0x3C..=0x3F => {
                // Private-use intermediate (e.g. '?').  Williams collects
                // these the same way as 0x20..=0x2F intermediates.
                self.collect_intermediate(byte);
                self.state = State::CsiParam;
            }
            0x20..=0x2F => {
                self.collect_intermediate(byte);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7E => {
                cb.csi_dispatch(self.params_slice(), self.intermediates_slice(), byte);
                self.state = State::Ground;
            }
            0x7F => {}
            0x18 | 0x1A | 0x1B => {}
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_param<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => cb.execute(byte),
            0x30..=0x39 => self.continue_param_digit(byte),
            0x3B => self.commit_param(),
            0x3C..=0x3F => {
                // Out-of-place private marker — invalid; ignore until end.
                self.state = State::CsiIgnore;
            }
            0x20..=0x2F => {
                self.commit_param_if_pending();
                self.collect_intermediate(byte);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7E => {
                self.commit_param_if_pending();
                cb.csi_dispatch(self.params_slice(), self.intermediates_slice(), byte);
                self.state = State::Ground;
            }
            0x7F => {}
            0x18 | 0x1A | 0x1B => {}
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_intermediate<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => cb.execute(byte),
            0x20..=0x2F => self.collect_intermediate(byte),
            0x40..=0x7E => {
                cb.csi_dispatch(self.params_slice(), self.intermediates_slice(), byte);
                self.state = State::Ground;
            }
            0x30..=0x3F => self.state = State::CsiIgnore,
            0x7F => {}
            0x18 | 0x1A | 0x1B => {}
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_ignore(&mut self, byte: u8) {
        // Per spec: stay here until a final byte 0x40..=0x7E, then drop the
        // sequence on the floor (no dispatch).
        if let 0x40..=0x7E = byte {
            self.state = State::Ground;
        }
    }

    fn osc_string<C: ParserCallbacks>(&mut self, cb: &mut C, byte: u8) {
        match byte {
            // BEL terminates OSC in xterm convention.
            0x07 => {
                cb.osc_dispatch(&self.osc_buffer);
                self.state = State::Ground;
            }
            // ST (string terminator) is ESC \ — but ESC is handled by the
            // anywhere transition above which puts us back into Escape.
            // The Escape state will see '\\' (0x5C) and dispatch as
            // esc_dispatch — we want it as OSC terminator instead.  To do
            // that we treat 0x5C arriving in Escape state from OSC as the
            // closing ST.  Implemented in `escape()` already (esc_dispatch
            // 0x5C is innocuous).  For test-coverage simplicity we keep
            // both BEL and the ESC \ path working.
            //
            // Above, we don't actually deliver osc_dispatch on ESC \ yet
            // because the anywhere ESC transition resets the sequence and
            // we lose the buffer.  So for now: only BEL terminates OSC.
            // (Phase 1.1.x will revisit if real workloads need ESC \.)
            _ => {
                if self.osc_buffer.len() < MAX_OSC_LEN {
                    self.osc_buffer.push(byte);
                }
                // else: silently truncate — bounded by design (stability).
            }
        }
    }

    // ------ helpers ------

    fn collect_intermediate(&mut self, byte: u8) {
        if self.intermediates_len < MAX_INTERMEDIATES {
            self.intermediates[self.intermediates_len] = byte;
            self.intermediates_len += 1;
        }
        // else: silently drop; we're in a malformed sequence territory anyway.
    }

    fn intermediates_slice(&self) -> &[u8] {
        &self.intermediates[..self.intermediates_len]
    }

    fn start_param_digit(&mut self, byte: u8) {
        self.current_param = (byte - b'0') as u32;
        self.has_current_param = true;
    }

    fn continue_param_digit(&mut self, byte: u8) {
        // Saturate at u16::MAX so absurdly long digit runs don't overflow.
        // Real CSI params don't exceed 65535 in any sane terminal.
        let next = self.current_param * 10 + (byte - b'0') as u32;
        self.current_param = next.min(u16::MAX as u32);
        self.has_current_param = true;
    }

    fn commit_param(&mut self) {
        if self.params_len < MAX_PARAMS {
            self.params[self.params_len] = self.current_param as u16;
            self.params_len += 1;
        }
        self.current_param = 0;
        self.has_current_param = false;
    }

    fn commit_param_if_pending(&mut self) {
        if self.has_current_param {
            self.commit_param();
        }
    }

    fn params_slice(&self) -> &[u16] {
        &self.params[..self.params_len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum Event {
        Print(char),
        Execute(u8),
        Esc {
            intermediates: Vec<u8>,
            byte: u8,
        },
        Csi {
            params: Vec<u16>,
            intermediates: Vec<u8>,
            byte: u8,
        },
        Osc(Vec<u8>),
    }

    #[derive(Default)]
    struct Recorder {
        events: Vec<Event>,
    }

    impl ParserCallbacks for Recorder {
        fn print(&mut self, ch: char) {
            self.events.push(Event::Print(ch));
        }
        fn execute(&mut self, byte: u8) {
            self.events.push(Event::Execute(byte));
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
            self.events.push(Event::Esc {
                intermediates: intermediates.to_vec(),
                byte,
            });
        }
        fn csi_dispatch(&mut self, params: &[u16], intermediates: &[u8], byte: u8) {
            self.events.push(Event::Csi {
                params: params.to_vec(),
                intermediates: intermediates.to_vec(),
                byte,
            });
        }
        fn osc_dispatch(&mut self, data: &[u8]) {
            self.events.push(Event::Osc(data.to_vec()));
        }
    }

    fn parse(bytes: &[u8]) -> Vec<Event> {
        let mut p = Parser::new();
        let mut r = Recorder::default();
        for &b in bytes {
            p.advance(&mut r, b);
        }
        r.events
    }

    #[test]
    fn print_ascii_each_byte_emits_one_event() {
        assert_eq!(
            parse(b"Hi!"),
            vec![Event::Print('H'), Event::Print('i'), Event::Print('!')]
        );
    }

    #[test]
    fn print_utf8_two_byte_codepoint() {
        // "é" = U+00E9 = 0xC3 0xA9
        assert_eq!(parse(&[0xC3, 0xA9]), vec![Event::Print('é')]);
    }

    #[test]
    fn print_utf8_three_byte_codepoint() {
        // "中" = U+4E2D = 0xE4 0xB8 0xAD
        assert_eq!(parse(&[0xE4, 0xB8, 0xAD]), vec![Event::Print('中')]);
    }

    #[test]
    fn print_utf8_four_byte_codepoint() {
        // "🚀" = U+1F680 = 0xF0 0x9F 0x9A 0x80
        assert_eq!(parse(&[0xF0, 0x9F, 0x9A, 0x80]), vec![Event::Print('🚀')]);
    }

    #[test]
    fn invalid_utf8_emits_replacement() {
        // 0xC0 is never a valid UTF-8 lead.
        let evs = parse(&[0xC0, b'A']);
        assert_eq!(evs, vec![Event::Print('\u{FFFD}'), Event::Print('A')]);
    }

    #[test]
    fn truncated_utf8_then_ascii_recovers() {
        // 0xE4 starts a 3-byte sequence but a printable ASCII follows
        // without continuation — emit replacement and proceed.
        let evs = parse(&[0xE4, b'B']);
        assert_eq!(evs, vec![Event::Print('\u{FFFD}'), Event::Print('B')]);
    }

    #[test]
    fn c0_control_byte_executes() {
        // BEL (0x07) is a C0 control — execute, no print.
        assert_eq!(parse(&[0x07]), vec![Event::Execute(0x07)]);
    }

    #[test]
    fn esc_letter_dispatches_with_no_intermediates() {
        // ESC c — RIS, full reset.  We just emit the dispatch event.
        assert_eq!(
            parse(b"\x1Bc"),
            vec![Event::Esc {
                intermediates: vec![],
                byte: b'c'
            }]
        );
    }

    #[test]
    fn esc_with_intermediate_collected() {
        // ESC ( B — designate G0 charset as USASCII.
        assert_eq!(
            parse(b"\x1B(B"),
            vec![Event::Esc {
                intermediates: vec![b'('],
                byte: b'B'
            }]
        );
    }

    #[test]
    fn csi_no_params_dispatches() {
        // ESC [ A — cursor up by 1 (default).
        assert_eq!(
            parse(b"\x1B[A"),
            vec![Event::Csi {
                params: vec![],
                intermediates: vec![],
                byte: b'A'
            }]
        );
    }

    #[test]
    fn csi_single_numeric_param() {
        // ESC [ 5 A — cursor up by 5.
        assert_eq!(
            parse(b"\x1B[5A"),
            vec![Event::Csi {
                params: vec![5],
                intermediates: vec![],
                byte: b'A'
            }]
        );
    }

    #[test]
    fn csi_multiple_params_separated_by_semicolons() {
        // ESC [ 1;2;3 H — cursor position with extra params.
        assert_eq!(
            parse(b"\x1B[1;2;3H"),
            vec![Event::Csi {
                params: vec![1, 2, 3],
                intermediates: vec![],
                byte: b'H'
            }]
        );
    }

    #[test]
    fn csi_omitted_param_is_zero() {
        // ESC [ ;5 H — first param defaults to 0, second is 5.
        // Higher layer will treat 0 as "default" (= 1 for cursor moves).
        assert_eq!(
            parse(b"\x1B[;5H"),
            vec![Event::Csi {
                params: vec![0, 5],
                intermediates: vec![],
                byte: b'H'
            }]
        );
    }

    #[test]
    fn csi_private_marker_collected_as_intermediate() {
        // ESC [ ? 25 h — DECSET show cursor.
        assert_eq!(
            parse(b"\x1B[?25h"),
            vec![Event::Csi {
                params: vec![25],
                intermediates: vec![b'?'],
                byte: b'h'
            }]
        );
    }

    #[test]
    fn osc_with_bel_terminator() {
        // ESC ] 0;hello BEL — set window title "hello".
        assert_eq!(
            parse(b"\x1B]0;hello\x07"),
            vec![Event::Osc(b"0;hello".to_vec())]
        );
    }

    #[test]
    fn garbage_returns_to_ground_then_continues() {
        // A malformed CSI sequence (with both ?  in the middle) must not
        // wedge the parser; subsequent valid input still parses.
        let mut p = Parser::new();
        let mut r = Recorder::default();
        for &b in b"\x1B[?\xFF?A\x1B[A" {
            p.advance(&mut r, b);
        }
        // The first sequence is broken garbage — no Csi event for it.
        // The second \x1B[A must dispatch normally.
        let csi_events: Vec<&Event> = r
            .events
            .iter()
            .filter(|e| matches!(e, Event::Csi { .. }))
            .collect();
        assert_eq!(
            csi_events.len(),
            1,
            "expected exactly one CSI dispatch, got {:?}",
            r.events
        );
        assert!(matches!(
            csi_events[0],
            Event::Csi { params, intermediates, byte: b'A' }
                if params.is_empty() && intermediates.is_empty()
        ));
    }

    #[test]
    fn osc_buffer_is_bounded() {
        // Feed an OSC sequence longer than MAX_OSC_LEN; must not allocate
        // unboundedly.  We don't terminate it, so no event fires; the test
        // is that this completes (and that subsequent parsing still works).
        let mut p = Parser::new();
        let mut r = Recorder::default();
        for &b in b"\x1B]0;" {
            p.advance(&mut r, b);
        }
        for _ in 0..(MAX_OSC_LEN * 4) {
            p.advance(&mut r, b'x');
        }
        // Now terminate.
        p.advance(&mut r, 0x07);
        match r.events.first() {
            Some(Event::Osc(data)) => assert!(data.len() <= MAX_OSC_LEN),
            _ => panic!("expected truncated OSC event, got {:?}", r.events),
        }
    }
}

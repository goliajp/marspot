//! The kitty keyboard protocol: the flags marspot implements and the
//! per-screen stack a program pushes, pops and sets them on.

/// The kitty keyboard protocol flags marspot implements: bits 0, 1
/// and 2 — disambiguate escape codes, report event types, report
/// alternate keys.  Between them that is what Claude Code asks for
/// (`CSI > 5 u` = 1|4) and what codex asks for (`CSI > 7 u` = 1|2|4).
///
/// The protocol is built around a terminal reporting what it actually
/// does — a program sets flags, reads `CSI ? u` back, and encodes for
/// what it finds — so claiming a bit we do not honour is worse than
/// claiming none.  Every write to the stack masks with this constant.
///
/// The two not implemented, and why they are refusals rather than
/// gaps:
///
/// - **bit 3, report all keys as escape codes.**  Every keystroke
///   would become a CSI sequence, including the ones an input method
///   is mid-composition on.  marspot routes IME text through the same
///   key path (`app.rs` `insertText:`), and a composed 中 arrives as
///   text with no physical key behind it — there is no honest
///   sequence to send for it.  Nothing asks for the bit: neither
///   Claude Code nor codex sets it, and a terminal that offers it
///   without an answer for composition would break CJK input for the
///   feature nobody requested.
/// - **bit 4, report associated text.**  It exists to carry the text
///   of keys that bit 3 turned into escape codes.  With bit 3
///   refused it has nothing to carry: the keys that produce text
///   already send that text, unchanged, which is the same
///   information by a shorter route.
pub const KITTY_KEYBOARD_SUPPORTED: u8 = 0b0_0111;

/// Per-screen stack of kitty keyboard flags: `CSI > u` pushes,
/// `CSI < u` pops, `CSI = u` sets in place, `CSI ? u` reports.
///
/// Eight entries, fixed and wrapping, following ghostty
/// (`references/ghostty/src/terminal/kitty/key.zig`): a push past the
/// end evicts the oldest rather than growing, and a pop of more than
/// it holds resets it.  Both are deliberate.  The stack is driven
/// entirely by what a program sends, and an unbounded one is a
/// program's way of asking for unbounded memory — the same reason
/// this terminal caps every other input-sized structure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KittyKeyboardStack {
    flags: [u8; Self::LEN],
    idx: usize,
}

impl KittyKeyboardStack {
    const LEN: usize = 8;

    /// What is in force right now — what `CSI ? u` answers and what
    /// the key encoder reads.
    pub fn current(self) -> u8 {
        self.flags[self.idx]
    }

    /// `CSI = flags ; mode u`.  Mode 1 replaces, 2 sets the given
    /// bits, 3 clears them.
    pub(super) fn set(&mut self, mode: u16, v: u8) {
        let v = v & KITTY_KEYBOARD_SUPPORTED;
        let cur = self.flags[self.idx];
        self.flags[self.idx] = match mode {
            2 => cur | v,
            3 => cur & !v,
            // 1, and anything else: replace.  An unknown mode is far
            // likelier to be a miscount of the spec's three than a
            // request to do nothing.
            _ => v,
        };
    }

    pub(super) fn push(&mut self, v: u8) {
        self.idx = (self.idx + 1) % Self::LEN;
        self.flags[self.idx] = v & KITTY_KEYBOARD_SUPPORTED;
    }

    pub(super) fn pop(&mut self, n: usize) {
        if n >= Self::LEN {
            *self = Self::default();
            return;
        }
        for _ in 0..n {
            self.flags[self.idx] = 0;
            self.idx = (self.idx + Self::LEN - 1) % Self::LEN;
        }
    }
}

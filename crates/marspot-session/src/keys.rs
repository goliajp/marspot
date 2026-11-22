//! What L2 forwards as typed: a key, encoded with this terminal's
//! modes, and a paste.

use marspot_term::input_core::{MarspotKeyEvent, Modifiers};

use crate::SessionImpl;

/// Encode one forwarded keystroke with L3's own terminal mode flags,
/// write it to the PTY, and local-echo printable bytes ahead of the
/// round trip. Returns whether the echo painted the grid (so the caller
/// republishes even when no PTY bytes pumped this tick).
///
/// Clipboard reads resolve to `None` here: Cmd-V paste is forwarded by
/// L2 as already-resolved text in a later step, not pulled from the
/// pasteboard by the GUI-free L3.
pub(crate) fn handle_key(session: &mut SessionImpl, event: MarspotKeyEvent, mods: Modifiers) -> bool {
    let modes = session.terminal().input_modes();
    let Some(bytes) =
        marspot_term::input_core::key_event_to_bytes(&event, mods, modes, || None)
    else {
        return false;
    };
    let _ = session.write(&bytes);
    // Local-echo predict — but ONLY for plain printable runs.  An
    // escape sequence (ESC-leading) is a control code; the first
    // byte 0x1b isn't printable so predict_byte rejects it, but the
    // sequence's tail (`[`, `C`, …) is ASCII printable and would be
    // painted literally into the grid, producing the `[C[C[C` smear
    // the user reported every time they pressed an arrow key.  Just
    // skip the whole run when it starts with ESC.
    let mut predicted = false;
    if !bytes.starts_with(&[0x1b]) {
        for &b in bytes.as_ref() {
            if session.terminal_mut().predict_byte(b) {
                predicted = true;
            }
        }
    }
    predicted
}

/// Write pasted text (already resolved from the macOS pasteboard by L2) to
/// the PTY.  Wrap in bracketed-paste markers iff this terminal turned the
/// mode on (DECSET 2004) so the receiving app treats it as a single paste
/// rather than typed input; markers inside the text are dropped so it
/// cannot end the paste early.  No local predict — the PTY echo
/// round-trips back through the normal pump.
pub(crate) fn handle_paste(session: &mut SessionImpl, text: &str) {
    if session.terminal().bracketed_paste_mode() {
        let mut bytes = Vec::new();
        marspot_term::paste::bracketed_paste(text.as_bytes(), &mut bytes);
        let _ = session.write(&bytes);
    } else {
        let _ = session.write(text.as_bytes());
    }
}

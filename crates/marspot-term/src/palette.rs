//! The terminal's own default foreground / background.
//!
//! These live here, not in the renderer, because a program can ASK for
//! them: `OSC 10 ; ? BEL` and `OSC 11 ; ? BEL` are how a TUI finds out
//! whether it is drawing on a dark or a light terminal.  A query with
//! no answer is not neutral — an app that waits for one falls back to
//! a degraded guess (the same failure this codebase already hit with
//! DA1, where a missing reply produced extra blank rows and misaligned
//! chrome).  So the values have to be reachable from the emulator, and
//! there can only be one copy of them: the renderer reads these.

/// Background color for the terminal.  Near-pure-black with a
/// near-imperceptible navy tint — the user's preferred direction after
/// seeing iTerm2's #14191e default felt too grey in marspot's 9-grid
/// layout.  Both renderers paint with this constant so the BG matches
/// across the AppKit / Metal switch.
pub const BG: (f64, f64, f64) = (0.006, 0.008, 0.014);

/// Default foreground.  Matches iTerm2's "Foreground Color (Dark)" —
/// slightly off-white (`#dbdbdb`), softer than pure 0.92 grey on the
/// eyes for long-running sessions.
pub const FG: (f64, f64, f64) = (0.8620, 0.8620, 0.8620);

/// `rgb:RRRR/GGGG/BBBB`, the form xterm answers a color query in.
/// 16 bits per channel: the wire format is what it is, and rounding to
/// 8 and doubling would answer a question with less precision than was
/// asked.
pub fn xterm_rgb(c: (f64, f64, f64)) -> String {
    let q = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
    format!("rgb:{:04x}/{:04x}/{:04x}", q(c.0), q(c.1), q(c.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_color_is_answered_in_the_form_that_was_asked() {
        assert_eq!(xterm_rgb((0.0, 0.0, 0.0)), "rgb:0000/0000/0000");
        assert_eq!(xterm_rgb((1.0, 1.0, 1.0)), "rgb:ffff/ffff/ffff");
        // The real default foreground, at full precision.
        assert_eq!(xterm_rgb(FG), "rgb:dcab/dcab/dcab");
    }
}

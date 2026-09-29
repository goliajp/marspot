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

/// The cursor block's colour.  Here for the same reason `FG` and `BG`
/// are: `OSC 12 ; ? ST` asks for it.  The renderer draws the block
/// with an alpha of its own — how solid the cursor looks is a drawing
/// decision, and not part of what a program asked for.
pub const CURSOR: (u8, u8, u8) = (220, 224, 235);

/// ANSI 16-colour palette — punchier than iTerm2's stock Dark.
/// iTerm2 Dark's bright-red (#dc7974) and bright-magenta (#e07de0)
/// lean pink-salmon, and bright-blue (#a6aaf1) is lavender; the user
/// flagged these as "red looks pink, everything looks grey".  This
/// table keeps the dark variants similar (they're already grounded)
/// but bumps the bright row to saturated values — closer to macOS
/// Terminal.app's defaults and the One Dark / Tomorrow Night family.
///
/// Lives beside `FG` / `BG` rather than in the renderer because
/// `OSC 4 ; n ; ? ST` asks for these too, and a colour a program can
/// ask about cannot have two copies.
pub const ANSI_16: [(f64, f64, f64); 16] = [
    (0.0784, 0.0980, 0.1176), //  0 black           #14191e
    (0.7726, 0.2354, 0.1568), //  1 red             #c53c28
    (0.1875, 0.7813, 0.3398), //  2 green           #30c757
    (0.8125, 0.6172, 0.1602), //  3 yellow          #cf9e29
    (0.3320, 0.5391, 0.9023), //  4 blue            #5489e6
    (0.7344, 0.3672, 0.8125), //  5 magenta         #bb5ecf
    (0.1602, 0.7188, 0.7461), //  6 cyan            #29b7be
    (0.7810, 0.7811, 0.7810), //  7 white           #c7c7c7
    (0.4078, 0.4078, 0.4078), //  8 bright black    #676767
    (1.0000, 0.3711, 0.3398), //  9 bright red      #ff5f57 (was pink)
    (0.3203, 0.8633, 0.4297), // 10 bright green    #51dc6e
    (1.0000, 0.7656, 0.2148), // 11 bright yellow   #ffc337
    (0.3984, 0.6328, 1.0000), // 12 bright blue     #66a1ff
    (1.0000, 0.4453, 0.7813), // 13 bright magenta  #ff72c8 (was lavender)
    (0.3984, 0.9219, 0.9492), // 14 bright cyan     #66ebf2
    (1.0000, 1.0000, 1.0000), // 15 bright white    #feffff
];

/// The 256-colour palette: the 16 above, then the 6×6×6 cube, then
/// the 24-step grey ramp.  The cube and the ramp are xterm's, which
/// is what a program means when it sends `CSI 38;5;n m`.
pub fn indexed(idx: u8) -> (f64, f64, f64) {
    if (idx as usize) < ANSI_16.len() {
        return ANSI_16[idx as usize];
    }
    if idx < 232 {
        const RAMP: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let n = idx - 16;
        let r = RAMP[(n / 36) as usize];
        let g = RAMP[((n / 6) % 6) as usize];
        let b = RAMP[(n % 6) as usize];
        return (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    }
    let v = 8 + (idx - 232) as i32 * 10;
    let f = v as f64 / 255.0;
    (f, f, f)
}

/// Eight-bit channels as the unit floats the rest of this module uses.
pub fn unit(c: (u8, u8, u8)) -> (f64, f64, f64) {
    (
        c.0 as f64 / 255.0,
        c.1 as f64 / 255.0,
        c.2 as f64 / 255.0,
    )
}

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

    #[test]
    fn the_256_palette_is_xterms() {
        // Spot-checks against xterm's own table: the ends of the
        // cube, a middle cube entry, and the ends of the grey ramp.
        assert_eq!(xterm_rgb(indexed(16)), "rgb:0000/0000/0000");
        assert_eq!(xterm_rgb(indexed(231)), "rgb:ffff/ffff/ffff");
        // 196 = cube (5,0,0) = #ff0000.
        assert_eq!(xterm_rgb(indexed(196)), "rgb:ffff/0000/0000");
        // Grey ramp runs 8..238 in steps of 10.
        assert_eq!(xterm_rgb(indexed(232)), "rgb:0808/0808/0808");
        assert_eq!(xterm_rgb(indexed(255)), "rgb:eeee/eeee/eeee");
    }
}

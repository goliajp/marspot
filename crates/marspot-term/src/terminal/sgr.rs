//! CSI parameter lookup, the DEC special graphics set, and SGR:
//! turning a parameter list into cell attributes and colours.

use crate::grid::{CellAttrs, Color};

/// Look up a CSI parameter, treating `0` and "missing" both as the supplied
/// default — this matches the standard convention where omitted params and
/// explicit `0` are equivalent for cursor movement and most other CSIs.
pub(super) fn param(params: &[u16], idx: usize, default: u16) -> u16 {
    match params.get(idx).copied() {
        Some(0) | None => default,
        Some(n) => n,
    }
}

/// Look up a CSI parameter without folding `0` into the default.  ED and EL
/// use this convention: the explicit `0` is a real selector (= "from cursor
/// to end"), distinct from "omitted" which is also `0` here.
pub(super) fn param_raw(params: &[u16], idx: usize, default: u16) -> u16 {
    params.get(idx).copied().unwrap_or(default)
}

/// A blank ' ' stamped with the supplied attrs.  Used wherever new cells
/// appear (BCE for erase, scroll-fill for the new bottom row).  Apps like
/// vim and tmux rely on this — clearing or scrolling under a non-default
/// background must produce colored cells, not transparent ones.
/// Apply a CSI SGR (Select Graphic Rendition) sequence.  Empty params is
/// equivalent to `[0]` (reset), per the standard.
///
/// We walk the param list with an explicit index because 38/48 (extended
/// color) consume additional params depending on the second value.
/// DEC Special Graphics, the set `ESC ( 0` selects.
///
/// Only `_` through `~` are remapped; everything below is the same
/// ASCII it always was.  Without this a TUI drawing a box with it
/// prints `lqqqk` where the line should be.
///
/// A table rather than a match: thirty-odd match arms compile to a
/// comparison chain that cost 5 KB of binary, which the size budget
/// caught.  This is 128 bytes of rodata and an index.
const DEC_GRAPHICS: [char; 32] = [
    ' ',        // _
    '\u{25c6}', // ` ◆
    '\u{2592}', // a ▒
    '\u{2409}', // b ␉
    '\u{240c}', // c ␌
    '\u{240d}', // d ␍
    '\u{240a}', // e ␊
    '\u{00b0}', // f °
    '\u{00b1}', // g ±
    '\u{2424}', // h ␤
    '\u{240b}', // i ␋
    '\u{2518}', // j ┘
    '\u{2510}', // k ┐
    '\u{250c}', // l ┌
    '\u{2514}', // m └
    '\u{253c}', // n ┼
    '\u{23ba}', // o ⎺
    '\u{23bb}', // p ⎻
    '\u{2500}', // q ─
    '\u{23bc}', // r ⎼
    '\u{23bd}', // s ⎽
    '\u{251c}', // t ├
    '\u{2524}', // u ┤
    '\u{2534}', // v ┴
    '\u{252c}', // w ┬
    '\u{2502}', // x │
    '\u{2264}', // y ≤
    '\u{2265}', // z ≥
    '\u{03c0}', // { π
    '\u{2260}', // | ≠
    '\u{00a3}', // } £
    '\u{00b7}', // ~ ·
];

#[inline]
pub(super) fn dec_special_graphic(ch: char) -> char {
    let c = ch as u32;
    if (0x5F..=0x7E).contains(&c) {
        DEC_GRAPHICS[(c - 0x5F) as usize]
    } else {
        ch
    }
}

pub(super) fn apply_sgr(attrs: &mut CellAttrs, params: &[u16], subs: &[bool]) {
    if params.is_empty() {
        *attrs = CellAttrs::default();
        return;
    }
    let mut i = 0;
    while i < params.len() {
        // A parameter written with colons arrives as itself plus the
        // sub-parameters behind it, and means one thing: `4:3` is a
        // curly underline, not underline-then-italic.  Take the whole
        // group or none of it.
        let mut end = i + 1;
        while end < params.len() && subs.get(end).copied().unwrap_or(false) {
            end += 1;
        }
        if end > i + 1 {
            apply_sgr_group(attrs, &params[i..end]);
            i = end;
            continue;
        }
        match params[i] {
            0 => *attrs = CellAttrs::default(),
            1 => attrs.bold = true,
            2 => attrs.dim = true,
            3 => attrs.italic = true,
            4 => attrs.underline = true,
            5 => attrs.set_blink(true),
            7 => attrs.reverse = true,
            8 => attrs.set_invisible(true),
            // SGR 22 is "normal intensity" — clears BOTH bold and dim
            // per ECMA-48, not just bold. TUIs (claudecode dim spans)
            // emit `2 ... 22` pairs and expect 22 to fully restore.
            22 => {
                attrs.bold = false;
                attrs.dim = false;
            }
            23 => attrs.italic = false,
            24 => attrs.underline = false,
            25 => attrs.set_blink(false),
            27 => attrs.reverse = false,
            28 => attrs.set_invisible(false),
            // Standard 8-color foreground.
            n @ 30..=37 => attrs.fg = Color::indexed((n - 30) as u8),
            // Extended foreground: 38;5;n (256-color) or 38;2;r;g;b (RGB).
            38 => {
                if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                    attrs.fg = color;
                    i += consumed;
                }
            }
            39 => attrs.fg = Color::DEFAULT,
            n @ 40..=47 => attrs.bg = Color::indexed((n - 40) as u8),
            48 => {
                if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                    attrs.bg = color;
                    i += consumed;
                }
            }
            49 => attrs.bg = Color::DEFAULT,
            // Bright foreground (8–15).
            n @ 90..=97 => attrs.fg = Color::indexed(8 + (n - 90) as u8),
            n @ 100..=107 => attrs.bg = Color::indexed(8 + (n - 100) as u8),
            _ => {} // unknown / unimplemented SGR code: silently skip
        }
        i += 1;
    }
}

/// One SGR parameter that came with sub-parameters.
///
/// The colon forms are the ones that used to take the whole sequence
/// down with them, so what matters most here is that an unhandled
/// group is skipped whole rather than leaking its numbers back into
/// the flat path — `58:2::255:0:0` must not set the foreground.
fn apply_sgr_group(attrs: &mut CellAttrs, g: &[u16]) {
    match g[0] {
        // `4:0` off, `4:1`..`4:5` are styles this terminal draws as a
        // straight underline; the distinction is a renderer feature,
        // not an attribute we drop the sequence over.
        4 => attrs.underline = g.get(1).copied().unwrap_or(1) != 0,
        38 => {
            if let Some(c) = colour_from_group(&g[1..]) {
                attrs.fg = c;
            }
        }
        48 => {
            if let Some(c) = colour_from_group(&g[1..]) {
                attrs.bg = c;
            }
        }
        // 58 / 59 colour the underline itself, which this terminal has
        // no separate attribute for.  Skipped, deliberately.
        _ => {}
    }
}

/// The colour inside a `38:` / `48:` group.
///
/// `5:n` is the palette.  `2:r:g:b` and `2::r:g:b` are both direct
/// colour — the second carries an empty colour-space id, which the
/// parser commits as a zero, so the length says which form arrived.
fn colour_from_group(rest: &[u16]) -> Option<Color> {
    match rest.first().copied()? {
        5 => rest.get(1).map(|n| Color::indexed((*n).min(255) as u8)),
        2 => {
            let base = if rest.len() >= 5 { 2 } else { 1 };
            let r = *rest.get(base)?;
            let g = *rest.get(base + 1)?;
            let b = *rest.get(base + 2)?;
            Some(Color::rgb(
                r.min(255) as u8,
                g.min(255) as u8,
                b.min(255) as u8,
            ))
        }
        _ => None,
    }
}

/// Parse the tail of a 38/48 sequence.  Returns the resulting `Color` and
/// the number of *additional* params consumed beyond the 38/48 itself, so
/// the caller can advance its index.
///
/// 5;n           → Indexed(n) — 256-color palette
/// 2;r;g;b       → Rgb(r,g,b) — direct color
/// anything else → None (caller leaves attrs unchanged and advances 1)
fn parse_extended_color(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first().copied()? {
        5 => {
            let n = rest.get(1).copied()?;
            Some((Color::indexed(n.min(255) as u8), 2))
        }
        2 => {
            let r = rest.get(1).copied()?;
            let g = rest.get(2).copied()?;
            let b = rest.get(3).copied()?;
            Some((
                Color::rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8),
                4,
            ))
        }
        _ => None,
    }
}

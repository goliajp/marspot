//! DEC private modes, and the screen state that rides with them: what
//! `CSI ? Pm h` / `l` change, what DECRQM reports, and the palette a
//! program set with OSC 4.

use super::{Handler, MouseTrackingMode};

/// Modes and screen state that no hot path reads on every byte: they
/// sit behind one reference so the print path pays nothing for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenModes {
    /// DECAWM (`?7`).  Off: a glyph printed in the last column stays
    /// there and the next one overwrites it.
    pub autowrap: bool,
    /// DECKPAM (`ESC =`) against DECKPNM (`ESC >`): the keypad sends
    /// `ESC O` sequences instead of what its keys type.
    pub keypad_app: bool,
    /// DECSCNM (`?5`): the whole screen is shown in reverse video.
    pub reverse_screen: bool,
    /// `?12`: the cursor blinks.  Recorded and reported; the cursor
    /// is drawn steady either way.
    pub cursor_blink: bool,
    /// `?1034`, xterm's meta mode.  Recorded and reported; Option
    /// sends `ESC` before the key in both states, which is what xterm
    /// does with `metaSendsEscape` on.
    pub meta_mode: bool,
    pub palette: PaletteOverrides,
}

impl ScreenModes {
    /// Bits 15-19 of a snapshot's mode word: DECAWM off, DECKPAM,
    /// DECSCNM, `?12`, `?1034`.  Off is the bit for DECAWM so an older
    /// snapshot reads as on.
    pub(super) fn to_bits(&self) -> u32 {
        (!self.autowrap as u32) << 15
            | (self.keypad_app as u32) << 16
            | (self.reverse_screen as u32) << 17
            | (self.cursor_blink as u32) << 18
            | (self.meta_mode as u32) << 19
    }

    /// The modes [`Self::to_bits`] wrote, with `palette`, which a
    /// snapshot does not carry.
    pub(super) fn from_bits(bits: u32, palette: PaletteOverrides) -> Self {
        ScreenModes {
            autowrap: bits & (1 << 15) == 0,
            keypad_app: bits & (1 << 16) != 0,
            reverse_screen: bits & (1 << 17) != 0,
            cursor_blink: bits & (1 << 18) != 0,
            meta_mode: bits & (1 << 19) != 0,
            palette,
        }
    }
}

impl Default for ScreenModes {
    fn default() -> Self {
        ScreenModes {
            autowrap: true,
            keypad_app: false,
            reverse_screen: false,
            cursor_blink: false,
            meta_mode: false,
            palette: PaletteOverrides::default(),
        }
    }
}

/// Palette entries a program replaced with OSC 4, over the theme's.
#[derive(Debug, PartialEq, Eq)]
pub struct PaletteOverrides {
    /// `0x0100_0000 | rgb` for an entry that is set, 0 for one that is not.
    entries: Box<[u32; 256]>,
    set: u16,
    /// Moves on every change, so a frame writer can tell it has to
    /// publish again without comparing the table.
    generation: u64,
}

impl Default for PaletteOverrides {
    fn default() -> Self {
        PaletteOverrides { entries: Box::new([0; 256]), set: 0, generation: 0 }
    }
}

impl Clone for PaletteOverrides {
    fn clone(&self) -> Self {
        PaletteOverrides { entries: self.entries.clone(), set: self.set, generation: self.generation }
    }

    /// Copies into the table already held: a frame writer takes a copy
    /// on every change, and the table is a kilobyte.
    fn clone_from(&mut self, source: &Self) {
        self.entries.clone_from(&source.entries);
        (self.set, self.generation) = (source.set, source.generation);
    }
}

const SET: u32 = 0x0100_0000;

impl PaletteOverrides {
    pub fn get(&self, i: u8) -> Option<(u8, u8, u8)> {
        let e = self.entries[i as usize];
        (e & SET != 0).then_some(((e >> 16) as u8, (e >> 8) as u8, e as u8))
    }

    pub fn is_empty(&self) -> bool {
        self.set == 0
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn set(&mut self, i: u8, (r, g, b): (u8, u8, u8)) {
        let e = &mut self.entries[i as usize];
        if *e & SET == 0 {
            self.set += 1;
        }
        *e = SET | (r as u32) << 16 | (g as u32) << 8 | b as u32;
        self.generation += 1;
    }

    pub fn reset(&mut self, i: u8) {
        let e = &mut self.entries[i as usize];
        if *e & SET != 0 {
            *e = 0;
            self.set -= 1;
            self.generation += 1;
        }
    }

    pub fn reset_all(&mut self) {
        if self.set != 0 {
            *self.entries = [0; 256];
            self.set = 0;
            self.generation += 1;
        }
    }
}

/// An X11 colour spec as OSC 4 carries it: `rgb:R/G/B` with one to
/// four hex digits a component, or `#RGB` with one to four a
/// component.  `None` for anything else, which leaves the entry alone.
pub(super) fn parse_color_spec(spec: &[u8]) -> Option<(u8, u8, u8)> {
    let hex = |d: &[u8]| -> Option<u32> {
        if d.is_empty() || d.len() > 4 {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(d).ok()?, 16).ok()
    };
    if let Some(rest) = spec.strip_prefix(b"rgb:") {
        let mut it = rest.split(|&b| b == b'/');
        let mut c = [0u8; 3];
        for slot in &mut c {
            let d = it.next()?;
            let v = hex(d)?;
            *slot = (v * 255 / ((1u32 << (4 * d.len())) - 1)) as u8;
        }
        return it.next().is_none().then_some((c[0], c[1], c[2]));
    }
    let rest = spec.strip_prefix(b"#")?;
    if rest.is_empty() || rest.len() % 3 != 0 || rest.len() > 12 {
        return None;
    }
    let n = rest.len() / 3;
    let mut c = [0u8; 3];
    for (k, slot) in c.iter_mut().enumerate() {
        let v = hex(&rest[k * n..(k + 1) * n])?;
        // the high bits, as X11 reads this form
        *slot = if n == 1 { (v << 4) as u8 } else { (v >> (4 * (n - 2))) as u8 };
    }
    Some((c[0], c[1], c[2]))
}

impl<'a> Handler<'a> {
    /// What DECRQM reports for `mode`: 0 not recognised, 1 set,
    /// 2 reset, 3 permanently set, 4 permanently reset.
    ///
    /// Mirrors `dec_mode` arm for arm — a mode this terminal acts on
    /// answers with its real state, and one it does not answers 0 so
    /// the program stays on whatever it does without us.
    pub(super) fn dec_mode_report(&self, mode: u16) -> u16 {
        let on = |b: bool| if b { 1 } else { 2 };
        match mode {
            1 => on(*self.cursor_key_app_mode),
            // DECSCLM — scrolling is always the jump kind
            4 => 4,
            5 => on(self.modes.reverse_screen),
            6 => on(*self.origin_mode),
            7 => on(self.modes.autowrap),
            12 => on(self.modes.cursor_blink),
            25 => on(*self.cursor_visible),
            47 | 1047 | 1049 => on(self.saved_main.is_some()),
            1000 => on(*self.mouse_tracking_mode == MouseTrackingMode::X11),
            1002 => on(*self.mouse_tracking_mode == MouseTrackingMode::ButtonEvent),
            1003 => on(*self.mouse_tracking_mode == MouseTrackingMode::AnyEvent),
            1004 => on(*self.focus_reporting),
            1006 => on(*self.mouse_sgr_encoding),
            1007 => on(*self.alt_scroll),
            1034 => on(self.modes.meta_mode),
            2004 => on(*self.bracketed_paste),
            2026 => on(*self.sync_output),
            _ => 0,
        }
    }

    pub(super) fn dec_mode(&mut self, mode: u16, set: bool) {
        match mode {
            // DECCKM — cursor keys send `ESC O X` in app mode, `ESC [ X`
            // otherwise. Read by the input layer for arrow encoding.
            1 => *self.cursor_key_app_mode = set,
            // DECCOLM — see `column_mode_reset`; set and reset do the
            // same thing here, because the difference between them is
            // the column count we do not own.
            3 => self.column_mode_reset(),
            // DECSCLM: smooth scrolling is not offered, and reset is
            // the state already in force.
            4 => {}
            5 => self.modes.reverse_screen = set,
            // DECOM.  Changing it homes the cursor, to the top margin
            // when it is on, because a program that switches addressing
            // modes and then draws has to start from a row both it and
            // the terminal agree on.
            6 => {
                *self.origin_mode = set;
                self.cursor_to_origin();
            }
            // DECAWM.  Turning it off also drops a wrap already armed:
            // the next glyph lands on the last column, as it would have
            // with the mode off from the start.
            7 => {
                self.modes.autowrap = set;
                if !set {
                    *self.pending_wrap = false;
                }
            }
            12 => self.modes.cursor_blink = set,
            // DECTCEM — cursor visibility.
            25 => *self.cursor_visible = set,
            // smcup/rmcup — alt screen + save/restore cursor.  ?1047
            // and ?47 are older variants; we accept them as aliases.
            // `?1049` is `?1047` plus `?1048`: the cursor is saved
            // before the switch and restored after coming back, which
            // is what lets a TUI leave the shell's prompt where it was
            // and printing the charset it had.  `?1047` and the older
            // `?47` switch buffers only; a program using those does
            // its own DECSC when it wants one.
            1049 => {
                if set {
                    self.save_cursor();
                    self.enter_alt_screen();
                } else {
                    self.exit_alt_screen();
                    self.restore_cursor();
                }
            }
            1047 | 47 => {
                if set {
                    self.enter_alt_screen();
                } else {
                    self.exit_alt_screen();
                }
            }
            // `?1048` is DECSC and DECRC by another spelling.
            1048 => {
                if set {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            // DECSET ?2004 — bracketed paste. When set, the input
            // layer wraps Cmd-V paste in `\e[200~ ... \e[201~` so apps
            // can distinguish paste from interactive typing. Read via
            // `Terminal::bracketed_paste_mode()`.
            2004 => *self.bracketed_paste = set,
            // DECSET ?1004 — focus reporting.  Turning it OFF also
            // forgets what was last reported, so a program that turns
            // it back on is told the current state rather than being
            // held to a stale one.
            1004 => {
                *self.focus_reporting = set;
                if !set {
                    *self.focus_reported = None;
                }
            }
            // 1000 / 1002 / 1003 — mouse reporting.  A TUI such as
            // claudecode sets one of these (with 1006 for SGR encoding)
            // on entering the alternate screen and expects wheel and
            // click events written back to the PTY as escape sequences;
            // with these ignored, the wheel never scrolls inside it.
            1000 => {
                *self.mouse_tracking_mode = if set {
                    MouseTrackingMode::X11
                } else {
                    MouseTrackingMode::Off
                }
            }
            1002 => {
                *self.mouse_tracking_mode = if set {
                    MouseTrackingMode::ButtonEvent
                } else {
                    MouseTrackingMode::Off
                }
            }
            1003 => {
                *self.mouse_tracking_mode = if set {
                    MouseTrackingMode::AnyEvent
                } else {
                    MouseTrackingMode::Off
                }
            }
            1006 => *self.mouse_sgr_encoding = set,
            // DEC 1007 — alternate scroll: the wheel is the arrow
            // keys on this screen.  See `Terminal::alt_scroll`.
            1007 => *self.alt_scroll = set,
            1034 => self.modes.meta_mode = set,
            // DEC 2026 — synchronized output.  `h` opens a batch, `l`
            // closes it; the presenter holds frames in between so a
            // half-drawn screen is never shown.  See
            // `Terminal::sync_output`.
            2026 => {
                *self.sync_output = set;
                if set {
                    *self.uses_sync_output = true;
                }
            }
            // 1015 / 2031 — silently accept but no-op.
            // 2031 (colour-scheme change notification) is accepted and
            // not reported on yet.  The palette does change with the
            // theme; a program that wants the new colours asks again
            // with OSC 10 / 11 / 4 / 12 and is answered from the
            // palette then in force.
            1015 | 2031 => {}
            _ => {} // unhandled DEC private mode — silently skip
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_clone_from_keeps_its_table() {
        let mut p = PaletteOverrides::default();
        p.set(1, (1, 2, 3));
        let mut copy = p.clone();
        let table = &*copy.entries as *const [u32; 256];
        p.set(2, (4, 5, 6));
        copy.clone_from(&p);
        assert_eq!(copy, p);
        assert_eq!(&*copy.entries as *const [u32; 256], table);
    }

    #[test]
    fn colour_spec_with_a_long_component_is_refused() {
        assert_eq!(parse_color_spec(b"rgb:123456789/0/0"), None);
        assert_eq!(parse_color_spec(b"rgb:12345678/0/0"), None);
        assert_eq!(parse_color_spec(b"rgb:ffff/0/8"), Some((255, 0, 136)));
        let mut t = super::super::Terminal::new(10, 2);
        t.feed(b"\x1b]4;1;rgb:123456789/0/0\x07");
        assert!(t.screen_modes().palette.is_empty());
    }
}

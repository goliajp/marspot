//! The terminal's own colours: default foreground and background, the
//! cursor, the ANSI 16 — one table per theme, and which theme is in
//! force.
//!
//! These live here, not in the renderer, because a program can ASK for
//! them: `OSC 10 ; ? BEL` and `OSC 11 ; ? BEL` are how a TUI finds out
//! whether it is drawing on a dark or a light terminal.  A query with
//! no answer is not neutral — an app that waits for one falls back to
//! a degraded guess (the same failure this codebase already hit with
//! DA1, where a missing reply produced extra blank rows and misaligned
//! chrome).  So the values have to be reachable from the emulator, and
//! there can only be one copy of them: the renderer reads these.
//!
//! The current theme is one atomic.  Whoever draws per cell takes
//! [`current`] once per frame and passes the reference down; the
//! emulator takes it once per query.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

/// Which table is in force.  The interface's own colour table is
/// indexed by the same id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ThemeId {
    Dark = 0,
    Light = 1,
    Contrast = 2,
}

impl ThemeId {
    pub const ALL: [ThemeId; 3] = [ThemeId::Dark, ThemeId::Light, ThemeId::Contrast];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Contrast => "contrast",
        }
    }

    pub fn parse(v: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str().eq_ignore_ascii_case(v.trim()))
    }

    fn from_u8(v: u8) -> Self {
        Self::ALL.get(v as usize).copied().unwrap_or(Self::Dark)
    }
}

/// One theme's terminal colours.
pub struct Palette {
    pub id: ThemeId,
    pub bg: (f64, f64, f64),
    pub fg: (f64, f64, f64),
    /// The cursor block's colour, as `OSC 12 ; ? ST` reports it.  The
    /// renderer draws the block with an alpha of its own — how solid
    /// the cursor looks is a drawing decision, and not part of what a
    /// program asked for.
    pub cursor: (u8, u8, u8),
    pub ansi: [(f64, f64, f64); 16],
}

const fn c8(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
}

/// Near-pure-black with a near-imperceptible navy tint — the user's
/// preferred direction after seeing iTerm2's #14191e default felt too
/// grey in marspot's 9-grid layout.  The foreground matches iTerm2's
/// "Foreground Color (Dark)" — slightly off-white (`#dbdbdb`), softer
/// than pure 0.92 grey on the eyes for long-running sessions.
///
/// The ANSI 16 are punchier than iTerm2's stock Dark.  iTerm2 Dark's
/// bright-red (#dc7974) and bright-magenta (#e07de0) lean pink-salmon,
/// and bright-blue (#a6aaf1) is lavender; the user flagged these as
/// "red looks pink, everything looks grey".  This table keeps the dark
/// variants similar (they're already grounded) but bumps the bright
/// row to saturated values — closer to macOS Terminal.app's defaults
/// and the One Dark / Tomorrow Night family.
pub static DARK: Palette = Palette {
    id: ThemeId::Dark,
    bg: (0.006, 0.008, 0.014),
    fg: (0.8620, 0.8620, 0.8620),
    cursor: (220, 224, 235),
    ansi: [
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
    ],
};

/// Paper white.  Every colour but the two whites is dark enough to
/// read as text on it, which is why none of them is the hue at full
/// strength: a terminal yellow that is yellow cannot be read on white.
pub static LIGHT: Palette = Palette {
    id: ThemeId::Light,
    bg: c8(255, 255, 255),
    fg: c8(28, 32, 40),
    cursor: (40, 44, 52),
    ansi: [
        c8(0, 0, 0),       //  0 black
        c8(190, 30, 30),   //  1 red
        c8(16, 120, 52),   //  2 green
        c8(140, 96, 0),    //  3 yellow
        c8(26, 95, 205),   //  4 blue
        c8(160, 45, 170),  //  5 magenta
        c8(0, 118, 136),   //  6 cyan
        c8(190, 194, 200), //  7 white
        c8(95, 99, 108),   //  8 bright black
        c8(214, 48, 40),   //  9 bright red
        c8(20, 134, 62),   // 10 bright green
        c8(160, 110, 0),   // 11 bright yellow
        c8(40, 108, 232),  // 12 bright blue
        c8(186, 56, 196),  // 13 bright magenta
        c8(0, 130, 150),   // 14 bright cyan
        c8(235, 237, 240), // 15 bright white
    ],
};

/// White on black, with every hue lifted until it clears 4.5:1 on the
/// ground.
pub static CONTRAST: Palette = Palette {
    id: ThemeId::Contrast,
    bg: c8(0, 0, 0),
    fg: c8(255, 255, 255),
    cursor: (255, 255, 255),
    ansi: [
        c8(0, 0, 0),       //  0 black
        c8(255, 110, 110), //  1 red
        c8(60, 230, 90),   //  2 green
        c8(255, 225, 60),  //  3 yellow
        c8(110, 170, 255), //  4 blue
        c8(255, 120, 255), //  5 magenta
        c8(60, 230, 240),  //  6 cyan
        c8(230, 230, 230), //  7 white
        c8(140, 140, 140), //  8 bright black
        c8(255, 150, 150), //  9 bright red
        c8(120, 255, 140), // 10 bright green
        c8(255, 240, 120), // 11 bright yellow
        c8(150, 195, 255), // 12 bright blue
        c8(255, 165, 255), // 13 bright magenta
        c8(130, 245, 250), // 14 bright cyan
        c8(255, 255, 255), // 15 bright white
    ],
};

pub fn of(id: ThemeId) -> &'static Palette {
    match id {
        ThemeId::Dark => &DARK,
        ThemeId::Light => &LIGHT,
        ThemeId::Contrast => &CONTRAST,
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(ThemeId::Dark as u8);
/// Counts changes of the current theme, so a process can tell that
/// one happened without remembering which theme it last drew.
static VERSION: AtomicU32 = AtomicU32::new(0);
/// What the setting says: a theme id, or `FOLLOW_SYSTEM`.
static CHOICE: AtomicU8 = AtomicU8::new(ThemeId::Dark as u8);
/// What `system` resolves to, as the shell last reported it.
static SYSTEM: AtomicU8 = AtomicU8::new(ThemeId::Dark as u8);
const FOLLOW_SYSTEM: u8 = u8::MAX;

pub fn current_id() -> ThemeId {
    ThemeId::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// The palette in force.  One atomic load: fine per frame and per
/// query, and the per-cell path is handed the reference instead.
pub fn current() -> &'static Palette {
    of(current_id())
}

pub fn version() -> u32 {
    VERSION.load(Ordering::Relaxed)
}

/// Whether the theme in force comes from the system's appearance
/// rather than from a theme the user named.
pub fn follows_system() -> bool {
    CHOICE.load(Ordering::Relaxed) == FOLLOW_SYSTEM
}

fn resolve() {
    let choice = CHOICE.load(Ordering::Relaxed);
    let next = if choice == FOLLOW_SYSTEM { SYSTEM.load(Ordering::Relaxed) } else { choice };
    if CURRENT.swap(next, Ordering::Relaxed) != next {
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Put the setting in force: a named theme, or `None` to follow the
/// system.
pub fn set_choice(theme: Option<ThemeId>) {
    CHOICE.store(theme.map_or(FOLLOW_SYSTEM, |t| t as u8), Ordering::Relaxed);
    resolve();
}

/// Record what `system` resolves to.  It takes effect at once when
/// the setting follows the system, and is remembered when it does not.
pub fn set_system(theme: ThemeId) {
    SYSTEM.store(theme as u8, Ordering::Relaxed);
    resolve();
}

impl Palette {
    /// The 256-colour palette: this theme's 16, then the 6×6×6 cube,
    /// then the 24-step grey ramp.  The cube and the ramp are xterm's,
    /// which is what a program means when it sends `CSI 38;5;n m`.
    pub fn indexed(&self, idx: u8) -> (f64, f64, f64) {
        if (idx as usize) < self.ansi.len() {
            return self.ansi[idx as usize];
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

// The dark table under its old names, for the renderer until it
// takes the table itself.
pub const BG: (f64, f64, f64) = DARK.bg;
pub const FG: (f64, f64, f64) = DARK.fg;
pub const CURSOR: (u8, u8, u8) = DARK.cursor;
pub const ANSI_16: [(f64, f64, f64); 16] = DARK.ansi;
pub fn indexed(idx: u8) -> (f64, f64, f64) {
    DARK.indexed(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_color_is_answered_in_the_form_that_was_asked() {
        assert_eq!(xterm_rgb((0.0, 0.0, 0.0)), "rgb:0000/0000/0000");
        assert_eq!(xterm_rgb((1.0, 1.0, 1.0)), "rgb:ffff/ffff/ffff");
        // The real default foreground, at full precision.
        assert_eq!(xterm_rgb(DARK.fg), "rgb:dcab/dcab/dcab");
    }

    #[test]
    fn the_256_palette_is_xterms() {
        // Spot-checks against xterm's own table: the ends of the
        // cube, a middle cube entry, and the ends of the grey ramp.
        // The same in every theme: only the first sixteen differ.
        for id in ThemeId::ALL {
            let p = of(id);
            assert_eq!(xterm_rgb(p.indexed(16)), "rgb:0000/0000/0000");
            assert_eq!(xterm_rgb(p.indexed(231)), "rgb:ffff/ffff/ffff");
            // 196 = cube (5,0,0) = #ff0000.
            assert_eq!(xterm_rgb(p.indexed(196)), "rgb:ffff/0000/0000");
            // Grey ramp runs 8..238 in steps of 10.
            assert_eq!(xterm_rgb(p.indexed(232)), "rgb:0808/0808/0808");
            assert_eq!(xterm_rgb(p.indexed(255)), "rgb:eeee/eeee/eeee");
        }
    }

    /// A named theme wins over the system's; following the system
    /// takes whatever was last reported, including a report that
    /// arrived while a named theme was in force.
    #[test]
    fn the_setting_decides_between_a_named_theme_and_the_systems() {
        set_choice(Some(ThemeId::Light));
        assert_eq!(current().id, ThemeId::Light);
        let v = version();
        set_system(ThemeId::Contrast);
        assert_eq!(current_id(), ThemeId::Light, "a named theme ignores the system");
        assert_eq!(version(), v, "and nothing changed, so nothing is redrawn");
        set_choice(None);
        assert!(follows_system());
        assert_eq!(current_id(), ThemeId::Contrast);
        assert_eq!(version(), v + 1);
        set_system(ThemeId::Dark);
        assert_eq!(current_id(), ThemeId::Dark);
        set_choice(Some(ThemeId::Dark));
        assert_eq!(version(), v + 2, "the same theme by another route is not a change");
    }

    /// A program that asks again after a switch is told the new
    /// colours: background, foreground, cursor and a palette entry.
    #[test]
    fn color_queries_are_answered_from_the_theme_in_force() {
        let mut t = crate::terminal::Terminal::new(20, 5);
        let mut ask = |q: &[u8]| {
            t.feed(q);
            String::from_utf8(t.take_response()).unwrap()
        };
        for id in ThemeId::ALL {
            set_choice(Some(id));
            let p = of(id);
            assert_eq!(ask(b"\x1b]11;?\x07"), format!("\x1b]11;{}\x07", xterm_rgb(p.bg)));
            assert_eq!(ask(b"\x1b]10;?\x07"), format!("\x1b]10;{}\x07", xterm_rgb(p.fg)));
            assert_eq!(ask(b"\x1b]12;?\x07"), format!("\x1b]12;{}\x07", xterm_rgb(unit(p.cursor))));
            assert_eq!(ask(b"\x1b]4;1;?\x07"), format!("\x1b]4;1;{}\x07", xterm_rgb(p.ansi[1])));
        }
        assert_ne!(xterm_rgb(DARK.bg), xterm_rgb(LIGHT.bg));
    }

    #[test]
    fn a_theme_id_survives_its_own_spelling() {
        for id in ThemeId::ALL {
            assert_eq!(ThemeId::parse(id.as_str()), Some(id));
            assert_eq!(of(id).id, id);
        }
        assert_eq!(ThemeId::parse(" Light\n"), Some(ThemeId::Light));
        assert_eq!(ThemeId::parse("system"), None);
    }
}

//! Terminal cell grid + bounded scrollback.
//!
//! `Grid` is a passive container: a rectangle of `Cell`s, a cursor position,
//! and a ring of scrolled-off lines.  It exposes primitive operations
//! (`set_cell`, `set_cursor`, `scroll_up`) and lets the emulator layer
//! (`terminal::Handler`) implement VT semantics on top of them.
//!
//! Scrollback is a ring buffer with a fixed capacity allocated at
//! construction.  This is **load-bearing** for the project's "cannot get
//! slower over time" commitment: terminal output is unbounded, but our
//! memory cost is not.  Once the ring is full, oldest lines are evicted
//! O(1).  Disk-backed scrollback (truly unlimited history) is a later
//! phase; it will live behind the same `Grid` interface.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            attrs: CellAttrs::default(),
        }
    }
}

impl From<char> for Cell {
    fn from(ch: char) -> Self {
        Cell {
            ch,
            attrs: CellAttrs::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CellAttrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    /// SGR 2 — half-intensity / faint. Renderer dims the resolved fg
    /// (typical: multiply RGB by ~0.55). TUIs (claudecode tips column
    /// divider, dimmed help text) use this for "secondary" content.
    pub dim: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    /// "Use the default foreground/background" — the renderer picks the
    /// theme's default.  This is distinct from Indexed(0), which is the
    /// palette's first color.
    #[default]
    Default,
    /// Indexed palette entry.  0–7 = standard, 8–15 = bright variants,
    /// 16–255 = 256-color extended palette.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

/// Default scrollback capacity — 10 000 lines.  At 80 cols × ~12 bytes/cell
/// this is ~9 MB per terminal pre-allocated.  Tunable via
/// [`Grid::with_scrollback`].
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// East Asian Wide / Fullwidth / emoji cells occupy two grid columns.
/// Anything else is one.
///
/// Two sources of "wide":
/// 1. **East Asian Wide / Fullwidth** ranges — coarse range list of the
///    common Unicode blocks (CJK ideographs, Hangul, fullwidth forms…)
///    that have EastAsianWidth=W or F per UAX #11.
/// 2. **Emoji_Presentation=Yes** — every codepoint whose default
///    presentation is emoji, per UTS #51 emoji-data.txt.  This is the
///    authoritative answer for "is this an emoji" and resolves the
///    2026-06-15 user complaint: characters like ✅ (U+2705), ⭐
///    (U+2B50), ❌ (U+274C) sit in `0x2000..0x2FFF` and are NOT in any
///    East Asian Wide block, so the old logic gave them width 1 and the
///    emoji font drew them at em-box width — clipping half the glyph.
///    See [`crate::emoji_presentation`] for the generated lookup.
///
/// Not yet handled:
/// - **VS15 (U+FE0E) / VS16 (U+FE0F)** — a text-default codepoint like
///   ⚠ (U+26A0) followed by VS16 should become emoji (width 2).  This
///   requires the parser to look ahead one codepoint and adjust width
///   on-the-fly — separate from per-char width.
/// - ZWJ sequences, regional indicators (flag pairs), modifier bases.
/// How much of the East-Asian Ambiguous table should be rendered at
/// width 2.
///
/// Off by default, and that default is load-bearing: every other
/// wcwidth in the stack (zsh, less, claudecode's Node `string-width`,
/// Python's `wcwidth`) treats Ambiguous as **narrow**, so widening
/// here makes marspot disagree with the program drawing the screen —
/// a cumulative CUP offset, and the picture falls out of sync after a
/// handful of edits.
///
/// The cost of the default is that a glyph designed for two cells is
/// squeezed into one.  Measured 2026-08-08: PingFang's `①` has a
/// 11.71 px ink box against a 7.20 px cell, so `rasterise_glyph`
/// scale-to-fits it to 61 % — and because circled digits are square,
/// the *width* is always the binding constraint.  No font fixes this:
/// the narrowest `①` on this machine (STIXGeneral, 8.21 px) still
/// lands at the same ~7.2 px on screen.  **Matching CJK size requires
/// two cells; there is no other lever.**
///
/// Letting the glyph overflow into a blank neighbour
/// (`may_overflow_cell` in the renderer) fixes the *isolated* case
/// without any disagreement at all, and that is what the rest of the
/// Ambiguous table gets.  But it cannot fix a **run**: in `①②③` every
/// neighbour is itself a glyph, so there is nothing to borrow and the
/// three come out at different sizes depending on what follows them.
/// Uniform full size for N square glyphs in N one-cell slots is not
/// available — they would overlap by 4.5 px each.  Two cells is the
/// only way, and it is a disagreement by construction.
///
/// Widening the circled family was tried as the default on
/// 2026-08-08 and **reverted the same hour**.  The prediction was that
/// drift would be confined to lines *containing* `①` — annoying but
/// bounded.  On a real screen it was not bounded: the disagreement
/// moves the **wrap point**, so a paragraph that merely scrolled past
/// a circled digit came back with stray characters stranded in the
/// left margin and the input box drawn over itself.  A terminal that
/// loses its wrapping is not trading a little alignment for a nicer
/// glyph; it is broken.
///
/// `Bun.stringWidth('①')` is 1 — claudecode is a Bun binary and that
/// is the function it uses — and agreeing with the program drawing
/// the screen is worth more than any glyph size.  The isolated case
/// is served instead by letting the glyph overflow a blank neighbour
/// (`may_overflow_cell`), which costs no disagreement at all; a *run*
/// of them stays small, and that is the price of a correct screen.
///
/// The circled half is a **setting** (`appearance.circled_wide`,
/// off) rather than a constant, because which of the two hurts more —
/// a run of unreadably small glyphs, or a wrap point that disagrees
/// with the program drawing the screen — is genuinely the user's call.
/// The panel writes its cost next to it.  The env var still overrides,
/// and still adds the `1` level that no panel offers.
///
///   MARSPOT_AMBIGUOUS_WIDE=0        everything narrow
///   MARSPOT_AMBIGUOUS_WIDE=circled  only ①②③ ❶❷❸ ⓪ … wide
///   MARSPOT_AMBIGUOUS_WIDE=1        the whole Ambiguous table wide
///
/// One-shot OnceLock load: per-process env var read at first call,
/// then a single relaxed branch on the hot path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AmbiguousWide {
    Off,
    Circled,
    All,
}

fn ambiguous_wide_mode() -> AmbiguousWide {
    // The env override is read once — it names this process, this run,
    // and cannot change under it.  The setting is read live, because
    // the panel is allowed to change it while the terminal is open.
    use std::sync::OnceLock;
    static ENV: OnceLock<Option<AmbiguousWide>> = OnceLock::new();
    let env = *ENV.get_or_init(|| match std::env::var("MARSPOT_AMBIGUOUS_WIDE") {
        Ok(v) if v == "0" => Some(AmbiguousWide::Off),
        Ok(v) if v.eq_ignore_ascii_case("circled") => Some(AmbiguousWide::Circled),
        Ok(v) if !v.is_empty() => Some(AmbiguousWide::All),
        _ => None,
    });
    if let Some(mode) = env {
        return mode;
    }
    // The per-byte mirror, not `get()` — this runs once per parsed
    // character and a lock here is a measurable parse regression.
    if crate::settings::circled_wide() {
        AmbiguousWide::Circled
    } else {
        AmbiguousWide::Off
    }
}

/// The circled / enclosed alphanumerics, as one range test.
///
/// Deliberately wider than the Ambiguous table's own slice of them
/// (`0x2460..=0x24E9`): `⓪` (U+24EA) and the parenthesised and
/// double-circled tail through U+24FF are the same family and the same
/// complaint, and the dingbat set `❶..➓` is drawn to the same metrics.
pub fn is_enclosed_alphanumeric(cp: u32) -> bool {
    matches!(cp, 0x2460..=0x24FF | 0x2776..=0x2793)
}

/// East Asian Ambiguous (EAW=A per UAX #11) — narrow in Western
/// contexts, drawn wide by CJK fonts.
///
/// Public because it names exactly the population that gets *squeezed*:
/// we render these at one cell (see [`ambiguous_wide_mode`]) while the
/// font that owns the glyph designed it on a two-cell em square.  The
/// renderer uses this set to decide which glyphs may overflow their
/// cell rather than be scaled down to fit it.
pub fn is_ambiguous_width(ch: char) -> bool {
    let cp = ch as u32;
    matches!(
        cp,
        0x00A1 | 0x00A4 | 0x00A7..=0x00A8 | 0x00AA | 0x00AD..=0x00AE
        | 0x00B0..=0x00B4 | 0x00B6..=0x00BA | 0x00BC..=0x00BF
        | 0x00C6 | 0x00D0 | 0x00D7..=0x00D8 | 0x00DE..=0x00E1 | 0x00E6
        | 0x00E8..=0x00EA | 0x00EC..=0x00ED | 0x00F0 | 0x00F2..=0x00F3
        | 0x00F7..=0x00FA | 0x00FC | 0x00FE | 0x0101 | 0x0111 | 0x0113
        | 0x011B | 0x0126..=0x0127 | 0x012B | 0x0131..=0x0133
        | 0x0138 | 0x013F..=0x0142 | 0x0144 | 0x0148..=0x014B
        | 0x014D | 0x0152..=0x0153 | 0x0166..=0x0167 | 0x016B | 0x01CE
        | 0x01D0 | 0x01D2 | 0x01D4 | 0x01D6 | 0x01D8 | 0x01DA | 0x01DC
        | 0x0251 | 0x0261 | 0x02C4 | 0x02C7 | 0x02C9..=0x02CB | 0x02CD
        | 0x02D0 | 0x02D8..=0x02DB | 0x02DD | 0x02DF
        | 0x2010 | 0x2013..=0x2016 | 0x2018..=0x2019 | 0x201C..=0x201D
        | 0x2020..=0x2022 | 0x2024..=0x2027 | 0x2030 | 0x2032..=0x2033
        | 0x2035 | 0x203B | 0x203E | 0x2074 | 0x207F | 0x2081..=0x2084
        | 0x20AC | 0x2103 | 0x2105 | 0x2109 | 0x2113 | 0x2116
        | 0x2121..=0x2122 | 0x2126 | 0x212B | 0x2153..=0x2154
        | 0x215B..=0x215E | 0x2160..=0x216B | 0x2170..=0x2179
        | 0x2189
        | 0x2190..=0x2199 | 0x21B8..=0x21B9 | 0x21D2 | 0x21D4 | 0x21E7
        | 0x2200 | 0x2202..=0x2203 | 0x2207..=0x2208 | 0x220B
        | 0x220F | 0x2211 | 0x2215 | 0x221A | 0x221D..=0x2220 | 0x2223
        | 0x2225 | 0x2227..=0x222C | 0x222E | 0x2234..=0x2237
        | 0x223C..=0x223D | 0x2248 | 0x224C | 0x2252 | 0x2260..=0x2261
        | 0x2264..=0x2267 | 0x226A..=0x226B | 0x226E..=0x226F
        | 0x2282..=0x2283 | 0x2286..=0x2287 | 0x2295 | 0x2299
        | 0x22A5 | 0x22BF | 0x2312
        | 0x2460..=0x24E9      // ← circled digits / letters (user report)
        | 0x24EB..=0x24FF      // Enclosed Alphanumeric Supplement (tail)
        // NB: 0x2500..0x259F (Box Drawing + Block Elements) intentionally
        // OMITTED from the Ambiguous→Wide list.  UAX #11 categorises them
        // as Ambiguous, but every terminal in the world (including iTerm2
        // with "Ambiguous Characters are Double-Width" on) keeps them at
        // width 1 — otherwise the horizontal char ─ (U+2500) can't tile
        // edge-to-edge with the next ─ and tables / boxes / tmux dividers
        // split apart.  marspot already self-rasterises this range
        // (`box_drawing_arms`, `block_element_rects`) at exactly 1 cell;
        // making it wide here would double-spend the slot and break the
        // tiling.  User report 2026-06-15: "横线没连起来了".
        | 0x25A0..=0x25A1 | 0x25A3..=0x25A9 | 0x25B2..=0x25B3
        | 0x25B6..=0x25B7 | 0x25BC..=0x25BD | 0x25C0..=0x25C1
        | 0x25C6..=0x25C8 | 0x25CB | 0x25CE..=0x25D1 | 0x25E2..=0x25E5
        | 0x25EF | 0x2605..=0x2606 | 0x2609 | 0x260E..=0x260F
        | 0x2614..=0x2615 | 0x261C | 0x261E | 0x2640 | 0x2642
        | 0x2660..=0x2661 | 0x2663..=0x2665 | 0x2667..=0x266A
        | 0x266C..=0x266D | 0x266F | 0x269E..=0x269F | 0x26BE..=0x26BF
        | 0x26C4..=0x26CD | 0x26CF..=0x26E1 | 0x26E3 | 0x26E8..=0x26FF
        | 0x273D | 0x2776..=0x277F | 0x2B56..=0x2B59
        | 0x3248..=0x324F      // Enclosed CJK Letters and Months
        | 0xE000..=0xF8FF      // Private Use Area
        | 0xFFFD               // Replacement character
        | 0x1F100..=0x1F10A    // Enclosed Alphanumeric Supplement
        | 0x1F110..=0x1F12D
        | 0x1F130..=0x1F169
        | 0x1F170..=0x1F18D
        | 0x1F18F..=0x1F190
        | 0x1F19B..=0x1F1AC
    )
}

pub fn char_width(ch: char) -> u8 {
    let cp = ch as u32;
    if cp == 0 {
        // The trail half of a wide pair uses NUL as a sentinel; it has
        // no inherent width of its own.
        return 0;
    }
    let east_asian_wide = matches!(
        cp,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0x303E      // CJK Radicals … CJK Symbols & Punctuation
        | 0x3041..=0x33FF      // Hiragana, Katakana, …, CJK Compatibility
        | 0x3400..=0x4DBF      // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0xA000..=0xA4CF      // Yi Syllables
        | 0xAC00..=0xD7A3      // Hangul Syllables
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0xFE30..=0xFE4F      // CJK Compatibility Forms
        | 0xFF00..=0xFF60      // Halfwidth & Fullwidth Forms (fullwidth half)
        | 0xFFE0..=0xFFE6      // Fullwidth Sign Forms
        | 0x20000..=0x2FFFD    // CJK Extension B–F
        | 0x30000..=0x3FFFD    // CJK Extension G–H
    );
    // East Asian Ambiguous (EAW=A per UAX #11): chars that are narrow
    // in Western contexts but rendered wide in CJK fonts.  iTerm2
    // exposes this as "Ambiguous Characters are Double-Width" (default
    // ON when locale is CJK).  marspot is CJK-locale-first (lihao@golia.jp
    // is the only user) so default it to wide.
    //
    // The 2026-06-15 user report: circled digits like ① ② ③ (Enclosed
    // Alphanumerics, U+2460-U+24FF) rendered "好小好小" because we
    // squeezed a 2-cell-wide glyph into 1 cell — the rasteriser shrank
    // it to fit.  Same fix lifts geometric shapes (▲ ●), misc symbols
    // (☆ ★ ☀ ☁), and dingbats (✓ ✗) up to the right cell footprint.
    // Computed INSIDE the `All` arm on purpose: `is_ambiguous_width` is
    // a several-hundred-arm range match, and the other two modes never
    // read it.  Hoisted out (as it was until 2026-08-18) every printable
    // character paid for a table walk whose result the default mode
    // throws away.
    let ambiguous_wide = match ambiguous_wide_mode() {
        AmbiguousWide::Off => false,
        AmbiguousWide::Circled => is_enclosed_alphanumeric(cp),
        AmbiguousWide::All => is_ambiguous_width(ch) || is_enclosed_alphanumeric(cp),
    };
    if east_asian_wide || ambiguous_wide || crate::emoji_presentation::has_emoji_presentation(cp) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod char_width_tests {
    use super::char_width;

    #[test]
    fn circled_digits_default_narrow() {
        // U+2460..U+2473 = ① ② ③ … ⑳ (Enclosed Alphanumerics).
        // EAW=A per UAX #11, which every other wcwidth calls narrow —
        // measured, not assumed: `Bun.stringWidth('①')` is 1, and
        // claudecode is a Bun binary.  Disagreeing was tried as the
        // default and reverted within the hour: it moves the WRAP
        // POINT, which strands characters in the left margin of any
        // paragraph that scrolled past one.  See `ambiguous_wide_mode`.
        assert_eq!(char_width('①'), 1);
        assert_eq!(char_width('②'), 1);
        assert_eq!(char_width('⑳'), 1);
        assert_eq!(char_width('⓪'), 1);
        assert_eq!(char_width('❶'), 1);
    }

    /// The `circled` middle setting acts on this set, so its extent is
    /// the thing worth pinning — the mode itself is a one-shot
    /// `OnceLock` and cannot be flipped inside a test process.
    ///
    /// Deliberately wider than the Ambiguous table's own slice
    /// (`0x2460..=0x24E9`): `⓪` and the tail through U+24FF are the
    /// same family and the same size complaint.
    /// The width follows the setting, live.
    ///
    /// This is the one entry in the panel that changes the terminal's
    /// *semantics* rather than its looks, so it is the one worth
    /// pinning: the default must stay narrow (agreeing with every
    /// other wcwidth), and turning it on must reach `char_width`
    /// without a restart.
    #[test]
    fn the_circled_width_follows_the_setting() {
        // SAFETY: nextest runs one test per process.
        unsafe { std::env::remove_var("MARSPOT_AMBIGUOUS_WIDE") };

        crate::settings::set_for_test(crate::settings::Settings::default());
        assert_eq!(char_width('①'), 1, "default agrees with everyone else");

        crate::settings::set_for_test(crate::settings::Settings {
            appearance_circled_wide: true,
            ..crate::settings::Settings::default()
        });
        assert_eq!(char_width('①'), 2, "the setting reaches char_width");
        assert_eq!(char_width('⓪'), 2);
        assert_eq!(char_width('❶'), 2);
        // …and only that family.  The rest of the Ambiguous table
        // stays in agreement, because only the circled one appears in
        // runs where overflow cannot help.
        assert_eq!(char_width('★'), 1);
        assert_eq!(char_width('●'), 1);
        assert_eq!(char_width('°'), 1);

        crate::settings::set_for_test(crate::settings::Settings::default());
        assert_eq!(char_width('①'), 1, "and back again, with nothing restarted");
    }

    #[test]
    fn the_circled_set_covers_the_whole_family() {
        use super::is_enclosed_alphanumeric as f;
        for c in ['①', '⑳', 'Ⓐ', 'ⓐ', '⓪', '⓿', '❶', '➓'] {
            assert!(
                f(c as u32),
                "{c} U+{:04X} must be in the circled set",
                c as u32
            );
        }
        // Neighbours that are NOT: the arrow below the block, the
        // geometric shapes above it, and the box drawing that must
        // stay in a mono font whatever happens.
        for c in ['→', '★', '●', '─', '╭', '㉈'] {
            assert!(!f(c as u32), "{c} U+{:04X} must stay out of it", c as u32);
        }
    }

    #[test]
    fn circled_letters_default_narrow() {
        assert_eq!(char_width('Ⓐ'), 1); // U+24B6
        assert_eq!(char_width('ⓐ'), 1); // U+24D0
    }

    #[test]
    fn geometric_shapes_misc_default_narrow() {
        // ★ ☆ ● ▲ ▼ — Ambiguous, and these stay narrow: they are in
        // agreement with every other wcwidth, and the renderer's
        // overflow-into-a-blank-neighbour path covers the isolated
        // case without any disagreement at all.  Only the circled
        // family, which shows up in runs, is worth the drift.
        assert_eq!(char_width('★'), 1); // U+2605
        assert_eq!(char_width('☆'), 1); // U+2606
        assert_eq!(char_width('●'), 1); // U+25CF
        assert_eq!(char_width('▲'), 1); // U+25B2
        assert_eq!(char_width('▼'), 1); // U+25BC
    }

    #[test]
    fn enclosed_cjk_letters_and_months_stay_wide() {
        // U+3248..U+324F is inside the East Asian Wide range
        // 0x3041..=0x33FF — the unconditional Wide check fires
        // before the Ambiguous opt-in, so this stays width 2
        // regardless of MARSPOT_AMBIGUOUS_WIDE.
        assert_eq!(char_width('㉈'), 2);
    }

    #[test]
    fn ascii_letters_stay_narrow() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('A'), 1);
        assert_eq!(char_width('1'), 1);
        assert_eq!(char_width(' '), 1);
    }

    #[test]
    fn cjk_ideographs_stay_wide() {
        // Regression — the East Asian Wide path is unchanged.
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('精'), 2);
        assert_eq!(char_width('神'), 2);
    }

    #[test]
    fn box_drawing_stays_narrow() {
        // Regression for the 09:10 install hotfix.  ─ │ ┌ ┐ └ ┘ ├ ┤
        // ┬ ┴ ┼ all MUST be width 1 even though UAX #11 marks them
        // Ambiguous, otherwise tables / tmux dividers / claudecode's
        // own box chrome split apart.
        for ch in '\u{2500}'..='\u{257F}' {
            assert_eq!(
                char_width(ch),
                1,
                "box-drawing U+{:04X} must stay narrow",
                ch as u32
            );
        }
    }

    #[test]
    fn block_elements_stay_narrow() {
        // Same reason — ▀ ▄ █ ▌ ▐ etc. tile edge-to-edge at width 1.
        for ch in '\u{2580}'..='\u{259F}' {
            assert_eq!(
                char_width(ch),
                1,
                "block-element U+{:04X} must stay narrow",
                ch as u32
            );
        }
    }

    #[test]
    fn emoji_stays_wide() {
        // Emoji presentation path still wins.
        assert_eq!(char_width('⭐'), 2); // U+2B50
        assert_eq!(char_width('✅'), 2); // U+2705
    }
}

use crate::scrollback::Scrollback;

pub struct Grid {
    cols: u16,
    rows: u16,
    /// Cell storage is row-major but **rotated**: logical row 0 lives at
    /// physical row `top_row`, logical row 1 at `(top_row + 1) % rows`,
    /// etc.  This turns `scroll_up(1)` from a `(rows-1) × cols` memcpy
    /// (the displaced rows shift up by one) into an O(cols) blank +
    /// pointer bump, since the new top simply becomes the cell after
    /// the old top.  Every cell access does one extra add+mod, but
    /// scrolls are by far the more frequent shape on real workloads
    /// (`cat large.log` triggers one scroll per line, hundreds of
    /// thousands of times for a 32 MB file).
    cells: Vec<Cell>,
    /// Cursor as (col, row).  Always bounded to [0, cols-1] x [0, rows-1].
    cursor_col: u16,
    cursor_row: u16,
    /// Physical row index of logical row 0 (`0..rows`).  Bumped by
    /// `scroll_up`; reset to 0 by `resize`.
    top_row: u16,
    scrollback: Scrollback,
    /// Monotonically-increasing count of lines pushed into scrollback
    /// over this grid's lifetime.  Increments once per row inside
    /// `scroll_up`, so callers can observe "how many new lines have
    /// rolled into history since I last checked" without polling
    /// `scrollback_len` (which goes flat once the ring is full).
    /// Drives the selection-follows-content invariant in main.rs:
    /// after each pump, Marspot bumps the selection's abs coords by
    /// this delta so the highlight stays on the same content as it
    /// shifts up into scrollback.
    scroll_push_count: u64,
    /// Per-row "this row is a soft continuation of the previous row"
    /// flags — set by the emulator when DECAWM autowrap flows a
    /// logical line across rows, cleared on scroll-fill / region
    /// scrolls / explicit reset.  Indexed by **physical** row and
    /// rotated with `top_row` exactly like `cells`.  This is what
    /// lets `resize` re-wrap content instead of truncating it.
    wrapped: Vec<bool>,
    /// Continuation flags for scrollback lines, parallel to the
    /// `Scrollback` ring (front = oldest).  Lives here rather than in
    /// `Scrollback` so the disk variant's record format stays pure
    /// cells; the flags don't need to outlive the process any more
    /// than the anon-mmap ring does.  Kept in lockstep with ring
    /// eviction by trimming to `scrollback.len()` after each push.
    sb_wrapped: std::collections::VecDeque<bool>,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES)
    }

    pub fn with_scrollback(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        Self::with_scrollback_kind(
            cols,
            rows,
            Scrollback::memory(scrollback_lines, cols as usize),
        )
    }

    /// Construct with a caller-supplied scrollback (Memory or File).
    /// `Terminal::new` calls this so it can pick the File variant
    /// when `MARSPOT_SESSION_ID` is set without dragging the env
    /// check through the Grid API.
    pub fn with_scrollback_kind(cols: u16, rows: u16, scrollback: Scrollback) -> Self {
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        let cells = vec![Cell::default(); cols as usize * rows as usize];
        // Pre-size the flag mirror to the ring's capacity so the
        // per-scrolled-line push in `scroll_up` (per-byte-path hot
        // for `cat`) never reallocates.  1 byte per line — 10 KB at
        // the default 10 000-line scrollback.
        let sb_capacity = scrollback.capacity();
        Self {
            cols,
            rows,
            cells,
            cursor_col: 0,
            cursor_row: 0,
            top_row: 0,
            scrollback,
            scroll_push_count: 0,
            wrapped: vec![false; rows as usize],
            sb_wrapped: std::collections::VecDeque::with_capacity(sb_capacity + 1),
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }
    pub fn cursor(&self) -> (u16, u16) {
        (self.cursor_col, self.cursor_row)
    }

    /// Approximate resident bytes for the live grid (cell storage
    /// only — scalar fields are negligible).  Per-MARSPOT_PROFILE_RSS
    /// sampling.
    pub fn approx_bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<Cell>()
    }

    /// Approximate resident bytes held by this grid's scrollback.
    /// Memory variant: lazy-grown `Vec<Cell>` capacity.  Disk
    /// variant: bytes-worth of lines actually written (not the full
    /// mmap reservation) — see `Scrollback::approx_bytes` for why.
    pub fn scrollback_approx_bytes(&self) -> usize {
        self.scrollback.approx_bytes()
    }

    /// Translate a logical row to its physical index in `cells`.
    #[inline]
    fn phys_row(&self, logical: u16) -> usize {
        let r = self.top_row as usize + logical as usize;
        if r >= self.rows as usize {
            r - self.rows as usize
        } else {
            r
        }
    }

    pub fn cell(&self, col: u16, row: u16) -> Cell {
        debug_assert!(col < self.cols && row < self.rows);
        let pr = self.phys_row(row);
        self.cells[pr * self.cols as usize + col as usize]
    }

    /// Clamp-and-set the cursor.  Out-of-bounds values clamp to the last
    /// valid position; the cursor is always within `[0, cols) x [0, rows)`.
    pub fn set_cursor(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols - 1);
        self.cursor_row = row.min(self.rows - 1);
    }

    /// Overwrite a single cell.  Caller is responsible for valid coords.
    pub fn set_cell(&mut self, col: u16, row: u16, cell: Cell) {
        debug_assert!(col < self.cols && row < self.rows);
        let pr = self.phys_row(row);
        self.cells[pr * self.cols as usize + col as usize] = cell;
    }

    /// Mutable view of `n` consecutive cells of `row` starting at
    /// `col`.  One ring-index resolve for the whole run — the batched
    /// ground-state print hot path builds on this.  Caller guarantees
    /// `col as usize + n <= cols`.
    pub fn row_cells_mut(&mut self, col: u16, row: u16, n: usize) -> &mut [Cell] {
        debug_assert!(col as usize + n <= self.cols as usize && row < self.rows);
        let pr = self.phys_row(row);
        let base = pr * self.cols as usize + col as usize;
        &mut self.cells[base..base + n]
    }

    /// Overwrite `bytes.len()` consecutive cells of `row` starting at
    /// `col` with single-width ASCII glyphs sharing `attrs`.  Caller
    /// guarantees `col as usize + bytes.len() <= cols`.
    pub fn set_row_run_ascii(&mut self, col: u16, row: u16, attrs: CellAttrs, bytes: &[u8]) {
        let run = self.row_cells_mut(col, row, bytes.len());
        for (cell, &b) in run.iter_mut().zip(bytes) {
            *cell = Cell {
                ch: b as char,
                attrs,
            };
        }
    }

    /// Mark / unmark logical `row` as a soft continuation of the row
    /// above (DECAWM autowrap flowed a logical line across them).
    /// The emulator calls this from the deferred-wrap path; `resize`
    /// uses the flags to re-wrap instead of truncate.
    pub fn set_row_wrapped(&mut self, row: u16, wrapped: bool) {
        let pr = self.phys_row(row.min(self.rows - 1));
        self.wrapped[pr] = wrapped;
    }

    pub fn row_wrapped(&self, row: u16) -> bool {
        let pr = self.phys_row(row.min(self.rows - 1));
        self.wrapped[pr]
    }

    /// Continuation flag for scrollback line `idx` (same indexing as
    /// `scrollback_line`: 0 = oldest).
    ///
    /// The file-backed variant stores the flag beside the line, and
    /// that copy is the one to believe: `sb_wrapped` only holds what
    /// THIS process pushed, while the file holds everything the
    /// session ever wrote.  An L3 that re-execs itself starts a fresh
    /// mirror against a scrollback that is already thousands of lines
    /// long, so every index lands past the mirror's end and every row
    /// in the history answers "not a continuation".  Measured: the
    /// same row of the same `scrollback.bin` reads `true` from the
    /// process that wrote it and `false` from the one that reattached
    /// — after which a soft-wrapped URL in history stops merging, its
    /// first row keeps a truncated link and its second row has none
    /// (2026-09-08 field report).
    pub fn scrollback_wrapped(&self, idx: usize) -> bool {
        if self.scrollback.keeps_wrapped_flags() {
            return self.scrollback.wrapped_at(idx);
        }
        self.sb_wrapped.get(idx).copied().unwrap_or(false)
    }

    /// True when the row addressed by `(view_offset, viewport_row)` is
    /// a DECAWM soft-wrap continuation of the row physically above it.
    /// Parallels `cell_at_view`: same translation from viewport coords
    /// to either a live row or a scrollback line.  Out-of-range coords
    /// return false (no wrap signal == "treat as logical line break").
    ///
    /// Callers walking the viewport row-by-row use this to merge a
    /// soft-wrapped logical line into one unit (selection copy without
    /// the spurious `\n`, link scanning that survives the wrap, etc.).
    pub fn wrapped_at_view(&self, view_offset: u16, viewport_row: u16) -> bool {
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            return self.row_wrapped((rows - 1 - abs) as u16);
        }
        let from_end = abs - rows;
        let sb_len = self.scrollback_len();
        if from_end < sb_len {
            return self.scrollback_wrapped(sb_len - 1 - from_end);
        }
        false
    }

    /// Drop every continuation flag (live + scrollback).  Used by
    /// full-screen erase — once the screen is wiped, gluing the new
    /// content to pre-wipe history would corrupt reflow.
    pub fn clear_all_wrapped(&mut self) {
        self.wrapped.iter_mut().for_each(|w| *w = false);
        for w in self.sb_wrapped.iter_mut() {
            *w = false;
        }
    }

    /// Scroll the visible region up by `lines`.  The displaced top rows are
    /// pushed into scrollback (in order, oldest first) and the bottom
    /// `lines` rows are filled with `fill` — pass a Cell carrying the
    /// current SGR background to honor BCE.  The cursor is NOT moved.
    pub fn scroll_up(&mut self, lines: u16, fill: Cell) {
        if lines == 0 {
            return;
        }
        let lines = lines.min(self.rows);
        let cols = self.cols as usize;
        // For each scrolled-out line: snapshot its data into the scrollback
        // ring, then blank that physical row (it becomes the new bottom),
        // then advance `top_row` so the next logical row 0 is the row that
        // used to be logical row 1.  No rows-1 × cols memcpy any more.
        for _ in 0..lines {
            let pr = self.top_row as usize;
            let start = pr * cols;
            // F3+12.1 — back to dumb-store after F3+12 blank-skip
            // turned out to delete legit user blank lines (Enter on
            // empty prompt, intentional paragraph breaks, echo "",
            // ...).  iTerm2 / Alacritty / xterm don't filter and
            // we shouldn't either.  Spinner pollution from TUIs
            // like claudecode is a known trade-off; user can wipe
            // scrollback when it gets unwieldy.  We won't make a
            // policy guess that can't be undone at read time.
            self.scrollback
                .push_line_with_wrapped(&self.cells[start..start + cols], self.wrapped[pr]);
            self.sb_wrapped.push_back(self.wrapped[pr]);
            self.wrapped[pr] = false;
            for c in &mut self.cells[start..start + cols] {
                *c = fill;
            }
            self.top_row += 1;
            if self.top_row >= self.rows {
                self.top_row = 0;
            }
            self.scroll_push_count = self.scroll_push_count.saturating_add(1);
        }
        // Mirror ring eviction (and the capacity-0 alt-screen case):
        // the flags deque must never outgrow what the ring retains.
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
        }
    }

    /// Monotonic count of lines pushed into scrollback over this
    /// grid's lifetime.  See the field comment for the contract.
    pub fn scroll_push_count(&self) -> u64 {
        self.scroll_push_count
    }

    /// Region-bounded scroll up: shift rows in `top..=bot` upward by
    /// `lines`; new rows at the bottom of the region are blanked with
    /// `fill`. Caller is responsible for keeping `top <= bot < rows`.
    ///
    /// Scrollback is fed **iff the region starts at row 0**.  A region
    /// below a header is a window-internal shuffle and its rows go
    /// nowhere; a region anchored at the top of the screen is one that
    /// reserves rows at the BOTTOM (a TUI's input box and status line),
    /// and content leaving row 0 is leaving the screen upward — the
    /// same event `scroll_up` records.
    ///
    /// This is not a guess.  codex reserves its bottom rows exactly
    /// that way (`CSI 1;56 r`, `CSI 1;58 r`, `CSI 1;53 r` — 582 of
    /// them in one session, with 137 `CSI S`), and dropping those rows
    /// left its panes with **zero** scrollback: the only way back
    /// through the session was codex's own transcript key, which is
    /// slow, jumps, and shows raw uncollapsed output.  claudecode uses
    /// no scroll region at all, which is why the same pane in the same
    /// terminal felt completely different.
    ///
    /// Measured against the reference (2026-09-07): the same sequence
    /// in iTerm2 — region `1..rows-8`, 120 lines scrolled through it —
    /// leaves all 120 reachable in iTerm2's buffer, lines 001 through
    /// 120.  So this matches what the terminal we are chasing does.
    pub fn scroll_up_region(&mut self, top: u16, bot: u16, lines: u16, fill: Cell) {
        if lines == 0 || top > bot || bot >= self.rows {
            return;
        }
        let region_h = bot - top + 1;
        let lines = lines.min(region_h);
        let cols = self.cols as usize;
        // See the doc comment: a region anchored at row 0 scrolls
        // content off the top of the SCREEN, not merely off the top of
        // a band inside it.
        let feeds_scrollback = top == 0;
        for _ in 0..lines {
            if feeds_scrollback {
                let pr = self.phys_row(top);
                let start = pr * cols;
                self.scrollback
                    .push_line_with_wrapped(&self.cells[start..start + cols], self.wrapped[pr]);
                self.sb_wrapped.push_back(self.wrapped[pr]);
                self.scroll_push_count = self.scroll_push_count.saturating_add(1);
            }
            // Shift rows [top+1..=bot] up by one logical row.
            for r in top..bot {
                // Copy cells[phys_row(r+1)] → cells[phys_row(r)].
                let src = self.phys_row(r + 1);
                let dst = self.phys_row(r);
                let src_start = src * cols;
                let dst_start = dst * cols;
                // Avoid `&mut` aliasing: copy via an intermediate Vec
                // when src/dst overlap (they don't in this layout, but
                // be defensive).
                let row_data: Vec<Cell> = self.cells[src_start..src_start + cols].to_vec();
                self.cells[dst_start..dst_start + cols].copy_from_slice(&row_data);
            }
            // Continuation flags travel with their rows.
            //
            // They used to be wiped across the whole band before the
            // shift, on the reading that a region scroll is a
            // TUI-internal shuffle where they stop meaning anything.
            // That was written when a region scroll fed no scrollback.
            // Once `feeds_scrollback` arrived, the wipe was running
            // BEFORE the push — so every line a top-anchored region
            // scrolled into history was recorded as "not a
            // continuation", which is all of a codex pane's history.
            // Measured on a live `scrollback.bin`: 1024 lines, zero
            // flags.  A URL that wrapped up there then had a link on
            // its first row and none on its second (2026-09-08 field
            // report).
            for r in top..bot {
                let src = self.phys_row(r + 1);
                let dst = self.phys_row(r);
                self.wrapped[dst] = self.wrapped[src];
            }
            // The row that arrives at the region's top keeps its flag
            // only when the row it continued went to scrollback — then
            // the line above it is still there, one place further
            // back.  In a band below row 0 that predecessor is simply
            // discarded and the row above becomes an unrelated one
            // outside the region, so the flag would be a lie.
            if top > 0 {
                let top_phys = self.phys_row(top);
                self.wrapped[top_phys] = false;
            }
            // Blank the new bottom-of-region row.
            let bot_phys = self.phys_row(bot);
            self.wrapped[bot_phys] = false;
            for c in &mut self.cells[bot_phys * cols..bot_phys * cols + cols] {
                *c = fill;
            }
        }
        // Mirror ring eviction, exactly as `scroll_up` does: the flags
        // deque must never outgrow what the ring retains.
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
        }
    }

    /// Region-bounded scroll down: shift rows in `top..=bot` downward
    /// by `lines`; new rows at the top of the region are blanked with
    /// `fill`. Used by IL (insert line) and CSI T (scroll down).
    pub fn scroll_down_region(&mut self, top: u16, bot: u16, lines: u16, fill: Cell) {
        if lines == 0 || top > bot || bot >= self.rows {
            return;
        }
        let region_h = bot - top + 1;
        let lines = lines.min(region_h);
        let cols = self.cols as usize;
        // Same flag invalidation as scroll_up_region.
        for r in top..=bot {
            let pr = self.phys_row(r);
            self.wrapped[pr] = false;
        }
        for _ in 0..lines {
            // Shift rows [top..bot] down by one logical row.
            for r in (top..bot).rev() {
                let src = self.phys_row(r);
                let dst = self.phys_row(r + 1);
                let src_start = src * cols;
                let dst_start = dst * cols;
                let row_data: Vec<Cell> = self.cells[src_start..src_start + cols].to_vec();
                self.cells[dst_start..dst_start + cols].copy_from_slice(&row_data);
            }
            // Blank the new top-of-region row.
            let top_phys = self.phys_row(top);
            for c in &mut self.cells[top_phys * cols..top_phys * cols + cols] {
                *c = fill;
            }
        }
    }

    /// Return the cell at the given viewport position, honouring
    /// `view_offset` (lines scrolled up from live, 0 = live view).
    /// Pulls from the live grid for visible rows and from
    /// `scrollback.cell_at` for scrolled-up rows; returns a default
    /// cell past the oldest scrollback line.  Both renderers
    /// (`render.rs` AppKit, `render_metal.rs` Metal) call through
    /// here so they read the same view of the grid.
    pub fn cell_at_view(&self, view_offset: u16, col: u16, viewport_row: u16) -> Cell {
        let rows = self.rows() as usize;
        let abs = view_offset as usize + (rows - 1 - viewport_row as usize);
        if abs < rows {
            return self.cell(col, (rows - 1 - abs) as u16);
        }
        let from_end = abs - rows;
        let sb_len = self.scrollback_len();
        if from_end < sb_len {
            let sb_idx = sb_len - 1 - from_end;
            if let Some(c) = self.scrollback_cell(sb_idx, col) {
                return c;
            }
        }
        Cell::default()
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }
    /// F3+10 — flush File-variant BufWriter tail to kernel page
    /// cache before execv.  Memory / Disk no-op.  See
    /// `Scrollback::flush_for_handoff` for the rationale.
    pub fn scrollback_flush_for_handoff(&self) {
        self.scrollback.flush_for_handoff();
    }
    pub fn scrollback_capacity(&self) -> usize {
        self.scrollback.capacity()
    }
    /// B3 — hand back an off-thread search snapshot of the File-backed
    /// scrollback (returns None for Memory/Disk variants).  Used by
    /// the L3 main loop on `SearchScrollback` to feed an isolated
    /// view to the search worker thread.
    pub fn file_scrollback_snapshot(&self) -> Option<crate::scrollback::FileSnapshot> {
        self.scrollback.file_snapshot()
    }

    /// B4 — snapshot the currently-visible grid rows (oldest visible
    /// first, newest visible last) for off-thread search.  Each entry
    /// is `(row_cells, row_wrapped)` matching the same shape the
    /// scrollback-search engine consumes via `SearchSource::line` /
    /// `wrapped`.  Cells are owned (cloned) so the snapshot is fully
    /// independent of the live grid — the worker thread can iterate
    /// without locking and without racing the PTY pump.
    ///
    /// The cost is O(rows × cols) cell clones, paid once per
    /// `SearchRequest` event on the main loop.  Bounded by the live
    /// grid (typically ~50 × 200 = 10 k cells) — well inside the
    /// "single SearchRequest is a cold event" budget.
    pub fn live_grid_snapshot_for_search(&self) -> Vec<(Vec<Cell>, bool)> {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows as u16 {
            let pr = self.phys_row(r);
            let row_cells = self.cells[pr * cols..(pr + 1) * cols].to_vec();
            let wrapped = self.wrapped[pr];
            out.push((row_cells, wrapped));
        }
        out
    }
    /// One cell from scrollback by `(line_idx, col)`.  Hot path —
    /// avoids per-line allocation that the disk-backed variant
    /// would otherwise need to materialise a slice.
    pub fn scrollback_cell(&self, line_idx: usize, col: u16) -> Option<Cell> {
        self.scrollback.cell_at(line_idx, col as usize)
    }
    /// One whole scrollback line, owned.  For tests + the headless
    /// `--snapshot` path; production rendering uses `scrollback_cell`.
    pub fn scrollback_line(&self, idx: usize) -> Option<Vec<Cell>> {
        self.scrollback.line_to_vec(idx)
    }
    /// RFC-002 §8: contiguous range from scrollback for shelld
    /// `GetScrollbackPage` responses.  Returns each line paired
    /// with its autowrap-continuation flag (`true` = this line was
    /// produced by the parser when the previous line filled the
    /// width and overflowed onto a fresh row, NOT a logical
    /// newline).  Without that flag the receiver's later
    /// `Grid::resize` reflow can't tell logical lines from
    /// hard-wrapped ones and ends up truncating wide content to
    /// the narrowest width the grid ever saw.
    pub fn scrollback_read_page(&self, start: usize, count: usize) -> Vec<(Vec<Cell>, bool)> {
        let cells = self.scrollback.read_lines(start, count);
        let sb_len = self.scrollback.len();
        cells
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                // read_lines returns oldest-first; mirror that into
                // sb_wrapped which is indexed the same way.
                let idx = start + i;
                let wrapped = if idx < sb_len {
                    self.sb_wrapped.get(idx).copied().unwrap_or(false)
                } else {
                    false
                };
                (row, wrapped)
            })
            .collect()
    }

    /// RFC-002 §8 (step 8c): push a historic line into the tail of
    /// scrollback.  The line must be **older than any line currently
    /// in scrollback** — callers append history in chronological
    /// order before live data has had a chance to scroll anything
    /// off the visible grid.  Once live data starts evicting rows
    /// into scrollback, calling this would corrupt ordering (history
    /// would appear to be newer than already-scrolled-off live
    /// output).  Caller polices the invariant; this is a thin
    /// pass-through.
    ///
    /// `wrapped` is the line's autowrap-continuation flag from the
    /// source (L4 master grid).  Storing it parallel to the cells
    /// is what lets the eventual `reflow` recover the original
    /// logical lines when the user resizes the window — without
    /// it, every historic row looks like a hard newline and a
    /// shrink → grow round trip leaves long lines permanently
    /// chopped.
    pub fn push_historic_scrollback_line(&mut self, line: &[Cell], wrapped: bool) {
        // Go through the wrapped-aware path so the File variant
        // persists the flag in its record; Memory/Disk variants drop
        // the flag (their truth lives in `sb_wrapped` below).
        self.scrollback.push_line_with_wrapped(line, wrapped);
        self.sb_wrapped.push_back(wrapped);
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
        }
    }
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.sb_wrapped.clear();
    }

    /// Resize the visible grid **without losing content**.
    ///
    /// Rows-only change: cells and scrollback are preserved verbatim.
    /// Growing adds blank rows at the bottom; shrinking first drops
    /// blank rows from the bottom (below the cursor), then pushes
    /// rows from the top into scrollback — same as every mainstream
    /// terminal.
    ///
    /// Column change: full reflow.  Scrollback + live rows are
    /// gathered into logical lines using the autowrap continuation
    /// flags, re-wrapped at the new width (wide CJK/emoji pairs are
    /// never split), and redistributed across scrollback + the live
    /// grid.  The cursor follows its logical position.  This is what
    /// keeps a shrink → grow round trip lossless instead of leaving
    /// every line truncated at the narrowest width it ever saw.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        assert!(cols > 0 && rows > 0, "grid dimensions must be positive");
        if cols == self.cols {
            self.resize_rows_only(rows);
            return;
        }
        self.reflow(cols, rows);
    }

    /// Height-only resize: lossless by construction.
    fn resize_rows_only(&mut self, rows: u16) {
        let cols = self.cols as usize;
        if rows > self.rows {
            // Grow: append blank rows at the bottom.  De-rotate into a
            // fresh buffer so indexing stays simple.
            let mut new_cells = vec![Cell::default(); cols * rows as usize];
            let mut new_wrapped = vec![false; rows as usize];
            for r in 0..self.rows {
                let src = self.phys_row(r) * cols;
                let dst = r as usize * cols;
                new_cells[dst..dst + cols].copy_from_slice(&self.cells[src..src + cols]);
                new_wrapped[r as usize] = self.wrapped[self.phys_row(r)];
            }
            self.cells = new_cells;
            self.wrapped = new_wrapped;
            self.rows = rows;
            self.top_row = 0;
            return;
        }
        // Shrink: prefer dropping blank rows below the cursor; push
        // the remainder from the top into scrollback so nothing is
        // lost and the cursor stays on screen.
        let mut need = (self.rows - rows) as usize;
        let mut last_keep = self.rows - 1; // drop blank rows from the bottom
        while need > 0 && last_keep > self.cursor_row && self.row_is_blank(last_keep) {
            last_keep -= 1;
            need -= 1;
        }
        if need > 0 {
            let push = need as u16;
            self.scroll_up(push, Cell::default());
            self.cursor_row = self.cursor_row.saturating_sub(push);
        }
        let mut new_cells = vec![Cell::default(); cols * rows as usize];
        let mut new_wrapped = vec![false; rows as usize];
        for r in 0..rows {
            let src = self.phys_row(r) * cols;
            let dst = r as usize * cols;
            new_cells[dst..dst + cols].copy_from_slice(&self.cells[src..src + cols]);
            new_wrapped[r as usize] = self.wrapped[self.phys_row(r)];
        }
        self.cells = new_cells;
        self.wrapped = new_wrapped;
        self.rows = rows;
        self.top_row = 0;
        self.cursor_row = self.cursor_row.min(rows - 1);
    }

    fn row_is_blank(&self, row: u16) -> bool {
        let start = self.phys_row(row) * self.cols as usize;
        self.cells[start..start + self.cols as usize]
            .iter()
            .all(|c| *c == Cell::default())
    }

    /// Column-changing resize: gather → re-wrap → redistribute.
    fn reflow(&mut self, cols: u16, rows: u16) {
        let old_cols = self.cols as usize;
        let new_cols = cols as usize;

        // 1. Gather logical lines (oldest first): scrollback, then
        //    live rows.  A row whose continuation flag is set glues
        //    onto the previous row.  Only the final fragment of each
        //    logical line gets its trailing default-blank cells
        //    trimmed — interior fragments were full-width by
        //    definition of autowrap.
        let mut lines: Vec<Vec<Cell>> = Vec::new();
        let sb_len = self.scrollback.len();
        // (line_idx, char_offset) of the cursor within `lines`.
        let mut cursor_line = 0usize;
        let mut cursor_off = 0usize;
        {
            let absorb = |row: Vec<Cell>, wrapped: bool, lines: &mut Vec<Vec<Cell>>| {
                if wrapped && !lines.is_empty() {
                    let prev = lines.last_mut().unwrap();
                    // Drop wide-bump pad sentinels at the glue seam:
                    // trailing NULs that are NOT the trail half of a
                    // wide lead.  (A legit trail NUL always directly
                    // follows a width-2 char.)
                    while prev.last().is_some_and(|c| c.ch == '\0')
                        && !(prev.len() >= 2 && char_width(prev[prev.len() - 2].ch) == 2)
                    {
                        prev.pop();
                    }
                    prev.extend(row);
                } else {
                    lines.push(row);
                }
            };
            for i in 0..sb_len {
                let row = self.scrollback.line_to_vec(i).unwrap_or_default();
                absorb(row, self.scrollback_wrapped(i), &mut lines);
            }
            for r in 0..self.rows {
                let start = self.phys_row(r) * old_cols;
                let row = self.cells[start..start + old_cols].to_vec();
                let wrapped = self.wrapped[self.phys_row(r)];
                absorb(row, wrapped, &mut lines);
                if r == self.cursor_row {
                    cursor_line = lines.len() - 1;
                    cursor_off = lines.last().unwrap().len() - old_cols + self.cursor_col as usize;
                }
            }
        }
        // Trim trailing pure-default cells per logical line, then drop
        // trailing all-blank logical lines that sit BELOW the cursor —
        // they're the unused bottom of the screen, not content.
        for l in lines.iter_mut() {
            while l.last().is_some_and(|c| *c == Cell::default()) {
                l.pop();
            }
        }
        while lines.len() > cursor_line + 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        cursor_off = cursor_off.min(lines.get(cursor_line).map_or(0, |l| l.len()));

        // 2. Re-wrap into segments of `new_cols`, never splitting a
        //    wide-cell pair (lead + NUL trail) across the boundary.
        //    Track where the cursor's (line, offset) lands.
        let mut segs: Vec<(Vec<Cell>, bool)> = Vec::new(); // (cells, continuation)
        let mut cursor_seg = 0usize;
        let mut cursor_col_new = 0usize;
        for (li, line) in lines.iter().enumerate() {
            let mut start = 0usize;
            let mut first = true;
            loop {
                let remaining = line.len() - start;
                let mut take = remaining.min(new_cols);
                // Wide pair straddling the boundary: the trail NUL
                // would land at the start of the next segment.  Pull
                // the lead over instead (its column renders blank,
                // exactly like the emulator's deferred-wrap print).
                if take > 0 && take < remaining && line[start + take].ch == '\0' {
                    take -= 1;
                }
                // Degenerate 1-column grid with a wide pair: splitting
                // is the only way to make progress.
                if take == 0 && remaining > 0 {
                    take = 1;
                }
                let end = start + take;
                if li == cursor_line
                    && cursor_off >= start
                    && (cursor_off < end || (cursor_off == end && remaining <= new_cols))
                {
                    cursor_seg = segs.len();
                    cursor_col_new = cursor_off - start;
                }
                segs.push((line[start..end].to_vec(), !first));
                first = false;
                start = end;
                if start >= line.len() {
                    break;
                }
            }
        }
        if segs.is_empty() {
            segs.push((Vec::new(), false));
        }

        // 3. Redistribute: the last `rows` segments become the live
        //    grid (top-anchored when everything fits); older segments
        //    refill the scrollback ring at the new width.
        let total = segs.len();
        let live_start = total.saturating_sub(rows as usize);
        // A fragment that underfills its row because the next wide
        // char was bumped to the following segment pads its tail with
        // NUL sentinels — the next gather drops them at the seam
        // instead of treating them as spaces.
        let pad_cell = |i: usize, segs: &[(Vec<Cell>, bool)]| -> Cell {
            if i + 1 < segs.len() && segs[i + 1].1 {
                Cell {
                    ch: '\0',
                    attrs: CellAttrs::default(),
                }
            } else {
                Cell::default()
            }
        };
        // F3+10f — restart wipes File scrollback (truncates bin/idx);
        // the loop below repopulates at new_cols from the gathered
        // segments.  Net effect for File: entire file gets rewritten
        // at the new width on every cols-change resize.  Cost is
        // bounded (the on-disk file's size at the old width); the
        // result is that historical content always matches the
        // current display width with no width-drift artifacts.
        self.scrollback.restart(new_cols);
        self.sb_wrapped.clear();
        for (i, (cells, cont)) in segs[..live_start].iter().enumerate() {
            let mut row = cells.clone();
            row.resize(new_cols, pad_cell(i, &segs));
            // Wrapped-aware push so File variant records keep the
            // reflow-derived continuation flag.
            self.scrollback.push_line_with_wrapped(&row, *cont);
            self.sb_wrapped.push_back(*cont);
        }
        while self.sb_wrapped.len() > self.scrollback.len() {
            self.sb_wrapped.pop_front();
        }
        let mut new_cells = vec![Cell::default(); new_cols * rows as usize];
        let mut new_wrapped = vec![false; rows as usize];
        for (r, (cells, cont)) in segs[live_start..].iter().enumerate() {
            let dst = r * new_cols;
            new_cells[dst..dst + cells.len()].copy_from_slice(cells);
            let pad = pad_cell(live_start + r, &segs);
            if pad.ch == '\0' {
                for c in &mut new_cells[dst + cells.len()..dst + new_cols] {
                    *c = pad;
                }
            }
            new_wrapped[r] = *cont;
        }
        self.cells = new_cells;
        self.wrapped = new_wrapped;
        self.cols = cols;
        self.rows = rows;
        self.top_row = 0;

        // 4. Cursor follows its logical position; if its segment got
        //    pushed into scrollback (screen shrank under it), clamp to
        //    the top-left of the live grid.
        if cursor_seg >= live_start {
            self.cursor_row = (cursor_seg - live_start).min(rows as usize - 1) as u16;
            self.cursor_col = (cursor_col_new as u16).min(cols - 1);
        } else {
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }
}

#[cfg(test)]
mod region_scrollback_tests {
    use super::*;

    /// A soft-wrap flag has to survive the scroll that files the row.
    ///
    /// The band's flags were wiped before the push, so every line a
    /// top-anchored region scrolled into history was recorded as "not
    /// a continuation" — all of a codex pane's history, measured at
    /// 1024 lines and zero flags on a live `scrollback.bin`.
    #[test]
    fn a_region_scroll_files_the_continuation_flag_it_was_given() {
        let mut g = Grid::new(20, 6);
        // Row 1 continues row 0; row 3 continues row 2.
        g.set_row_wrapped(1, true);
        g.set_row_wrapped(3, true);
        // codex's shape: a region anchored at row 0 with the bottom
        // rows reserved.  Scroll four rows out of it.
        g.scroll_up_region(0, 4, 4, Cell::default());
        assert_eq!(g.scrollback_len(), 4);
        let flags: Vec<bool> = (0..4).map(|i| g.scrollback_wrapped(i)).collect();
        assert_eq!(
            flags,
            [false, true, false, true],
            "each filed line keeps the flag its row carried",
        );
    }

    /// The row that arrives at the top of the band keeps its flag only
    /// when the row it continued went to scrollback.  In a band below
    /// row 0 that predecessor is discarded, so the flag would claim a
    /// continuation of whatever unrelated row sits above the region.
    #[test]
    fn a_band_below_row_zero_drops_the_flag_at_its_top() {
        let mut g = Grid::new(20, 6);
        g.set_row_wrapped(3, true);
        g.scroll_up_region(2, 5, 1, Cell::default());
        assert_eq!(g.scrollback_len(), 0, "a band below row 0 files nothing");
        assert!(
            !g.row_wrapped(2),
            "the row that moved up to the band's top continued a row that is gone",
        );

        // ...but the same shift one row further down keeps it: its
        // predecessor is still inside the band, one row above.
        let mut g = Grid::new(20, 6);
        g.set_row_wrapped(4, true);
        g.scroll_up_region(2, 5, 1, Cell::default());
        assert!(g.row_wrapped(3), "the flag travelled with its row");
    }

    /// The row blanked at the bottom of the band is new content, not a
    /// continuation of anything.
    #[test]
    fn the_blanked_bottom_row_is_not_a_continuation() {
        let mut g = Grid::new(20, 6);
        g.set_row_wrapped(5, true);
        g.scroll_up_region(0, 5, 1, Cell::default());
        assert!(!g.row_wrapped(5), "the blank row carries no flag");
    }

    /// The shape a TUI uses to reserve its input box: a region
    /// anchored at row 0 with rows held back at the bottom.
    fn scroll_through_region(rows: u16, reserved: u16, n: u32) -> Grid {
        let mut g = Grid::new(20, rows);
        let bot = rows - reserved - 1;
        for i in 1..=n {
            // Write at the region's last row, then scroll the region.
            g.set_cursor(0, bot);
            for (c, ch) in format!("LINE-{i:03}").chars().enumerate() {
                g.set_cell(c as u16, bot, Cell { ch, ..Default::default() });
            }
            g.scroll_up_region(0, bot, 1, Cell::default());
        }
        g
    }

    #[test]
    fn a_region_anchored_at_the_top_feeds_scrollback() {
        // Measured in iTerm2 with the identical sequence: all 120
        // lines stay reachable.  Dropping them is what left codex
        // panes with no history at all.
        let g = scroll_through_region(40, 8, 120);
        assert_eq!(g.scroll_push_count(), 120);
        let sb: Vec<String> = (0..g.scrollback_len())
            .filter_map(|i| g.scrollback_line(i))
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect();
        assert!(
            sb.iter().any(|l| l.starts_with("LINE-001")),
            "the oldest line must still be reachable"
        );
        // The newest line is still ON SCREEN, not in scrollback —
        // scrollback is where things go when they leave.
        let on_screen: Vec<String> = (0..40u16)
            .map(|r| (0..20u16).map(|c| g.cell(c, r).ch).collect::<String>())
            .collect();
        assert!(on_screen.iter().any(|l| l.starts_with("LINE-120")));
        assert!(
            !sb.iter().any(|l| l.starts_with("LINE-120")),
            "a line still on screen must not also be in history"
        );
    }

    #[test]
    fn a_region_below_a_header_does_not() {
        // A band inside the screen — a pager's body under a title.
        // Its rows go nowhere, exactly as before.
        let mut g = Grid::new(20, 40);
        for _ in 0..50 {
            g.scroll_up_region(3, 30, 1, Cell::default());
        }
        assert_eq!(g.scroll_push_count(), 0);
        assert_eq!(g.scrollback_len(), 0);
    }
}

#[cfg(test)]
mod tests {

    /// A soft-wrap flag has to survive the process that wrote it.
    ///
    /// `sb_wrapped` only holds what THIS process pushed; the file
    /// holds everything the session ever wrote.  An L3 that re-execs
    /// itself — which every silent update does — starts a fresh mirror
    /// against a scrollback already thousands of lines long, so every
    /// index lands past the mirror's end and all of history answers
    /// "not a continuation".  The visible cost was a URL that wrapped
    /// in history: its first row kept a truncated link and its second
    /// row had none.
    #[test]
    fn a_wrapped_flag_in_the_file_survives_the_process_that_wrote_it() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-grid-wrapped-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let bin = dir.join("scrollback.bin");
        let idx = dir.join("scrollback.idx");
        let (cols, rows) = (8u16, 3u16);

        let pushed = {
            let sb = crate::scrollback::Scrollback::file(bin.clone(), idx.clone(), cols as usize, 8)
                .expect("open");
            let mut g = Grid::with_scrollback_kind(cols, rows, sb);
            // Row 1 is a continuation of row 0; scroll both out.
            g.set_row_wrapped(1, true);
            g.scroll_up(3, Cell::default());
            assert!(g.scrollback_wrapped(1), "the writer sees its own flag");
            g.scrollback_len()
        };

        let sb = crate::scrollback::Scrollback::file(bin, idx, cols as usize, 8).expect("reopen");
        let g = Grid::with_scrollback_kind(cols, rows, sb);
        assert_eq!(g.scrollback_len(), pushed, "the file kept the lines");
        assert!(
            g.scrollback_wrapped(1),
            "a process that reattached to the same scrollback must read the same flag",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn empty_grid_is_all_default_cells_with_origin_cursor() {
        let g = Grid::new(80, 24);
        assert_eq!(g.cols(), 80);
        assert_eq!(g.rows(), 24);
        assert_eq!(g.cursor(), (0, 0));
        for r in 0..24 {
            for c in 0..80 {
                assert_eq!(g.cell(c, r), Cell::default());
            }
        }
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn set_cursor_clamps_to_grid_bounds() {
        let mut g = Grid::new(10, 5);
        g.set_cursor(99, 99);
        assert_eq!(g.cursor(), (9, 4));
    }

    #[test]
    fn scroll_up_one_pushes_top_row_to_scrollback() {
        let mut g = Grid::new(3, 2);
        g.set_cell(0, 0, 'a'.into());
        g.set_cell(1, 0, 'b'.into());
        g.set_cell(2, 0, 'c'.into());
        g.set_cell(0, 1, 'd'.into());
        g.set_cell(1, 1, 'e'.into());
        g.set_cell(2, 1, 'f'.into());

        g.scroll_up(1, Cell::default());

        // Row 0 became scrollback line 0, row 1 moved up to row 0, row 1 blank.
        let sb = g.scrollback_line(0).expect("scrollback[0] should exist");
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        assert_eq!(g.cell(0, 0).ch, 'd');
        assert_eq!(g.cell(2, 0).ch, 'f');
        assert_eq!(g.cell(0, 1), Cell::default());
        assert_eq!(g.scrollback_len(), 1);
    }

    #[test]
    fn scroll_up_uses_fill_cell_for_blank_bottom() {
        // BCE: caller passes a stamped Cell; the new bottom rows must adopt it.
        let mut g = Grid::new(3, 3);
        let blank = Cell {
            ch: ' ',
            attrs: CellAttrs {
                bg: Color::Indexed(1),
                ..CellAttrs::default()
            },
        };
        g.scroll_up(2, blank);
        for c in 0..3 {
            for r in 1..3 {
                assert_eq!(g.cell(c, r), blank);
            }
        }
    }

    #[test]
    fn scroll_up_more_than_height_clears_screen_and_pushes_all() {
        let mut g = Grid::new(2, 2);
        g.set_cell(0, 0, 'A'.into());
        g.set_cell(0, 1, 'B'.into());
        g.scroll_up(5, Cell::default()); // scroll farther than height
        // All 2 rows pushed (capped at rows).
        assert_eq!(g.scrollback_len(), 2);
        for r in 0..2 {
            for c in 0..2 {
                assert_eq!(g.cell(c, r), Cell::default());
            }
        }
    }

    #[test]
    fn scrollback_evicts_oldest_at_capacity() {
        let mut g = Grid::with_scrollback(2, 2, 3); // capacity 3 lines
        // Fill row 0 with 'X', row 1 blank, scroll once → scrollback[0] = "XX".
        // Repeat with markers to verify eviction order.
        for marker in ['1', '2', '3', '4'].iter() {
            g.set_cell(0, 0, Cell::from(*marker));
            g.set_cell(1, 0, Cell::from(*marker));
            g.scroll_up(1, Cell::default());
        }
        // After 4 pushes into a capacity-3 ring: '1' was evicted, ring holds
        // ['2','3','4'] in chronological order.
        assert_eq!(g.scrollback_len(), 3);
        let to_string = |line: &[Cell]| line.iter().map(|c| c.ch).collect::<String>();
        assert_eq!(to_string(&g.scrollback_line(0).unwrap()), "22");
        assert_eq!(to_string(&g.scrollback_line(1).unwrap()), "33");
        assert_eq!(to_string(&g.scrollback_line(2).unwrap()), "44");
        assert_eq!(g.scrollback_line(3), None);
    }

    #[test]
    fn clear_scrollback_zeroes_len_in_o1() {
        let mut g = Grid::with_scrollback(2, 2, 5);
        for _ in 0..5 {
            g.scroll_up(1, Cell::default());
        }
        assert_eq!(g.scrollback_len(), 5);
        g.clear_scrollback();
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.scrollback_line(0), None);
    }

    #[test]
    fn scroll_up_zero_is_noop() {
        let mut g = Grid::new(3, 2);
        g.set_cell(0, 0, 'A'.into());
        g.scroll_up(0, Cell::default());
        assert_eq!(g.cell(0, 0).ch, 'A');
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn zero_capacity_scrollback_silently_drops_pushes() {
        let mut g = Grid::with_scrollback(3, 2, 0);
        g.set_cell(0, 0, 'A'.into());
        g.scroll_up(1, Cell::default());
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.scrollback_capacity(), 0);
    }
}

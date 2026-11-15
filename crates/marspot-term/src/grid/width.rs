//! How many columns a character takes.

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
///
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
    // ON when locale is CJK).  marspot is CJK-locale-first, so the
    // default here is wide.
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
        let mut env = crate::test_env::EnvGuard::new();
        env.remove("MARSPOT_AMBIGUOUS_WIDE");

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

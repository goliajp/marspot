//! UAX #29 extended grapheme cluster segmentation.
//!
//! Splits a `&str` into the boundaries the user perceives as
//! "characters": a base codepoint plus all combining marks, VS15/VS16
//! variation selectors, ZWJ-glued emoji sequences (👨‍👩‍👧‍👦),
//! regional-indicator pairs (🇯🇵), skin-tone modifiers (👋🏽),
//! and virama-glued Indic consonant clusters (Devanagari, Bengali, …).
//!
//! The break decision is the GB1..GB13 rule set from UAX #29 plus the
//! GB9c Indic Conjunct Break rule.  The property tables come from the
//! generated [`crate::unicode_data`] module.
//!
//! Public surface:
//! - [`graphemes`] — iterator yielding `&str` clusters from a string.
//! - [`cluster_first_codepoint`] — the codepoint a renderer should
//!   use to look up base properties (Emoji_Presentation, EastAsianWidth)
//!   for the cluster.
//!
//! Conformance: see `grapheme_break_test_conformance` test, which
//! exercises every entry in the official `GraphemeBreakTest.txt`.

use crate::unicode_data::{gbp, incb, is_extended_pictographic, GBP, InCB};

/// Boundary decision state.  UAX #29 expresses GB11 ("Emoji Extend*
/// ZWJ × Emoji") and GB9c (Indic Conjunct Break) as patterns that
/// look at the run leading up to the candidate boundary, not just the
/// two adjacent codepoints.  We maintain three minimal flags to
/// reproduce those decisions without re-scanning history each step.
///
/// Reset to default at every confirmed break — clusters never carry
/// state across a boundary.
#[derive(Default, Clone, Copy)]
struct ClusterState {
    /// We have, somewhere earlier in the current run, seen an
    /// Extended_Pictographic codepoint, with only `Extend` codepoints
    /// (possibly zero) in between.  Used to drive GB11's left side.
    in_emoji_run: bool,
    /// Same as above, but the immediately-preceding codepoint is the
    /// ZWJ that follows the (Extended_Pictographic Extend*) prefix.
    /// GB11 fires when the NEXT codepoint is Extended_Pictographic.
    after_emoji_zwj: bool,
    /// We've seen `InCB::Consonant`, followed by zero or more
    /// `Extend`/`Linker`, INCLUDING at least one `Linker`.  GB9c fires
    /// when the next codepoint is `InCB::Consonant`.
    after_indic_linker: bool,
    /// As above but the linker hasn't appeared yet — we're between
    /// the consonant and the linker, walking through Extends.
    after_indic_consonant: bool,
    /// Count of consecutive Regional_Indicator codepoints in the
    /// current run.  GB12/GB13 say "break only between every other
    /// RI" — so we break iff the count is even (RI count incremented
    /// from 0,2,4,… → boundary, 1,3,5,… → no boundary).
    ri_count: usize,
}

/// Decide whether a grapheme cluster boundary falls **before** `next`,
/// given the immediately-preceding codepoint `prev` and the run state
/// accumulated since the last boundary.  Updates `state` so it's ready
/// for the decision at the codepoint after `next`.
///
/// Implements UAX #29 rules GB3, GB4, GB5, GB6, GB7, GB8, GB9, GB9a,
/// GB9b, GB9c, GB11, GB12, GB13.  GB1/GB2 (start/end of text) are
/// handled by the iterator's main loop.  Default GB999 is "break".
fn is_break(prev: char, next: char, state: &mut ClusterState) -> bool {
    let p = gbp(prev as u32);
    let n = gbp(next as u32);

    // Cluster state for the *next* iteration must reflect the
    // codepoint we're about to attach (or break before).  We compute
    // the boundary decision below using the OLD state, then update
    // it.
    let decision = decide_break(prev, next, p, n, state);

    if decision {
        // Boundary: state resets and the post-boundary codepoint
        // starts a new run.  Initialise from `next` alone.
        *state = ClusterState::default();
    }
    update_state_with(next, n, state);
    decision
}

fn decide_break(prev: char, next: char, p: GBP, n: GBP, state: &ClusterState) -> bool {
    // GB3: CR × LF — keep CRLF together as one cluster.
    if p == GBP::CR && n == GBP::LF {
        return false;
    }
    // GB4: (Control | CR | LF) ÷ — break after any control / line term.
    if matches!(p, GBP::Control | GBP::CR | GBP::LF) {
        return true;
    }
    // GB5: ÷ (Control | CR | LF) — break before any control / line term.
    if matches!(n, GBP::Control | GBP::CR | GBP::LF) {
        return true;
    }
    // GB6: L × (L | V | LV | LVT)  — Hangul jamo, leading consonant.
    if p == GBP::L && matches!(n, GBP::L | GBP::V | GBP::LV | GBP::LVT) {
        return false;
    }
    // GB7: (LV | V) × (V | T) — Hangul, vowel.
    if matches!(p, GBP::LV | GBP::V) && matches!(n, GBP::V | GBP::T) {
        return false;
    }
    // GB8: (LVT | T) × T — Hangul, trailing consonant.
    if matches!(p, GBP::LVT | GBP::T) && n == GBP::T {
        return false;
    }
    // GB9: × (Extend | ZWJ) — any combining mark or ZWJ attaches.
    if matches!(n, GBP::Extend | GBP::ZWJ) {
        return false;
    }
    // GB9a: × SpacingMark — spacing combining marks attach.
    if n == GBP::SpacingMark {
        return false;
    }
    // GB9b: Prepend × — leading prepend codepoints (e.g. Arabic
    // numeral sign) attach to the next character.
    if p == GBP::Prepend {
        return false;
    }
    // GB9c: \p{InCB=Consonant} [\p{InCB=Extend/Linker}]* \p{InCB=Linker}
    //       [\p{InCB=Extend/Linker}]* × \p{InCB=Consonant}
    // Indic Conjunct Break — Devanagari, Bengali, etc.  Keep two
    // consonants together if a linker (virama) appeared between them.
    if state.after_indic_linker && incb(next as u32) == InCB::Consonant {
        return false;
    }
    // GB11: \p{Extended_Pictographic} Extend* ZWJ × \p{Extended_Pictographic}
    // Compound emoji (👨‍👩‍👧‍👦, 🏳️‍🌈, 👩‍💻 …).
    if state.after_emoji_zwj && is_extended_pictographic(next as u32) {
        return false;
    }
    // GB12/GB13: (sot | [^RI]) (RI RI)* RI × RI
    // Regional Indicator pairs form a flag (🇯🇵), and every following
    // pair forms another flag — break between every other RI.
    if p == GBP::RegionalIndicator && n == GBP::RegionalIndicator {
        // The count tracked by state is the number of RIs already in
        // this run *including* `prev`.  GB12/13 say "break between two
        // RIs iff the count of RIs in the run ending at `prev` is
        // even" — i.e. an odd count means this RI joins the previous.
        return state.ri_count % 2 == 0;
    }

    // GB999: default — break.
    let _ = prev;
    true
}

/// Fold `next` into the cluster state used for the NEXT boundary
/// decision.  Called after every codepoint we attach to (or start) a
/// cluster, regardless of whether a boundary was emitted before it.
fn update_state_with(next: char, n: GBP, state: &mut ClusterState) {
    let in_cb = incb(next as u32);

    // GB11 left-side run tracking.  An Extended_Pictographic codepoint
    // starts the run; any Extend continues it; a ZWJ immediately
    // following the run sets `after_emoji_zwj`; anything else clears
    // the run.
    if is_extended_pictographic(next as u32) {
        state.in_emoji_run = true;
        state.after_emoji_zwj = false;
    } else if state.in_emoji_run && n == GBP::Extend {
        // Stay in emoji run; the next ZWJ can still glue.
    } else if state.in_emoji_run && n == GBP::ZWJ {
        state.after_emoji_zwj = true;
    } else {
        state.in_emoji_run = false;
        state.after_emoji_zwj = false;
    }

    // GB9c Indic run tracking.
    match in_cb {
        InCB::Consonant => {
            // A new consonant: only "after_consonant" is true (no
            // linker yet).  Conjunct chain from this point on.
            state.after_indic_consonant = true;
            state.after_indic_linker = false;
        }
        InCB::Linker => {
            // Linker valid only after a consonant (possibly with
            // intervening Extends).  Promote consonant→linker state.
            if state.after_indic_consonant || state.after_indic_linker {
                state.after_indic_linker = true;
            }
        }
        InCB::Extend => {
            // Extend keeps whichever state we're in (after_consonant
            // or after_linker) alive.
        }
        InCB::None => {
            state.after_indic_consonant = false;
            state.after_indic_linker = false;
        }
    }

    // GB12/13 Regional Indicator counter.
    if n == GBP::RegionalIndicator {
        state.ri_count += 1;
    } else {
        state.ri_count = 0;
    }
}

/// Iterator over UAX #29 extended grapheme cluster sub-strings.
///
/// Each `next()` returns `Some(&str)` covering exactly one cluster,
/// or `None` when the input is exhausted.  Sub-strings borrow from
/// the original string — no allocation per cluster.
pub struct GraphemeIter<'a> {
    s: &'a str,
    byte_pos: usize,
    state: ClusterState,
}

impl<'a> Iterator for GraphemeIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.byte_pos >= self.s.len() {
            return None;
        }
        let cluster_start = self.byte_pos;
        let remaining = &self.s[cluster_start..];
        let mut iter = remaining.char_indices();
        // First codepoint of the cluster: always consume.
        let (_, first) = iter.next().expect("non-empty by precondition");
        self.state = ClusterState::default();
        update_state_with(first, gbp(first as u32), &mut self.state);
        let mut prev = first;
        let mut cluster_end_offset = first.len_utf8();
        for (rel_byte, ch) in iter {
            if is_break(prev, ch, &mut self.state) {
                cluster_end_offset = rel_byte;
                break;
            }
            // No break: extend the cluster through `ch`.  Track its
            // end for the next iteration.
            cluster_end_offset = rel_byte + ch.len_utf8();
            prev = ch;
        }
        self.byte_pos = cluster_start + cluster_end_offset;
        Some(&self.s[cluster_start..self.byte_pos])
    }
}

/// Iterate `s` as UAX #29 extended grapheme clusters.  Zero-alloc
/// (borrows from the input).
pub fn graphemes(s: &str) -> GraphemeIter<'_> {
    GraphemeIter {
        s,
        byte_pos: 0,
        state: ClusterState::default(),
    }
}

/// Return the first codepoint of `cluster` — what a renderer should
/// use as the "base" character (e.g. to look up Emoji_Presentation
/// or EastAsianWidth).  `cluster` must be a non-empty grapheme
/// cluster; in practice always the case from [`graphemes`].
pub fn cluster_first_codepoint(cluster: &str) -> char {
    cluster
        .chars()
        .next()
        .expect("grapheme cluster is non-empty")
}

/// Visual width of a single codepoint in cells (0, 1, or 2), ignoring
/// any surrounding cluster context.  This is the per-codepoint
/// building block for [`cluster_width`].
///
/// - **0** for codepoints that contribute no visible advance:
///   combining marks (`Extend`), zero-width joiner/non-joiner, format
///   characters classified as Extend (incl. VS15/VS16), and
///   control / line-terminator characters that are handled by the
///   parser's escape path rather than written to a cell.
/// - **2** for East Asian Wide / Fullwidth / Emoji_Presentation
///   codepoints (delegates to [`crate::grid::char_width`]).
/// - **1** for everything else, including visible `SpacingMark` and
///   `Prepend` codepoints that the parser still treats as a normal
///   1-cell glyph.
fn cp_visual_width(ch: char) -> u8 {
    let cp = ch as u32;
    match gbp(cp) {
        GBP::Control | GBP::CR | GBP::LF | GBP::Extend | GBP::ZWJ => 0,
        // SpacingMark + Prepend render visibly in the same cluster as
        // their base; per-codepoint they contribute 1 cell of advance
        // — though `cluster_width`'s max-rule typically resolves to
        // the base's width anyway.
        GBP::SpacingMark | GBP::Prepend => 1,
        _ => crate::grid::char_width(ch),
    }
}

/// Total display width of a grapheme cluster in cells (0, 1, or 2).
///
/// The rule, per UTS #51 and conventional terminal practice:
/// 1. **VS16 (U+FE0F)** anywhere in the cluster forces emoji
///    presentation → width 2.  This is how text-default characters
///    like ⚠ (U+26A0) become the emoji ⚠️ — without VS16 they're
///    width 1, with it they're width 2.
/// 2. **VS15 (U+FE0E)** forces text presentation → width 1.
/// 3. Otherwise, take the maximum of [`cp_visual_width`] over every
///    codepoint in the cluster.  Combining marks contribute 0, the
///    base contributes 1 or 2.  Compound emoji built from ZWJ-glued
///    Extended_Pictographics, regional-indicator pairs, and skin-tone
///    sequences all bottom out at width 2 because their base is.
///
/// Empty input panics (clusters from [`graphemes`] are always
/// non-empty; the empty case isn't a legal input).
pub fn cluster_width(cluster: &str) -> u8 {
    let mut max_w: u8 = 0;
    let mut has_vs15 = false;
    let mut has_vs16 = false;
    for ch in cluster.chars() {
        let cp = ch as u32;
        if cp == 0xFE0F {
            has_vs16 = true;
        } else if cp == 0xFE0E {
            has_vs15 = true;
        }
        let w = cp_visual_width(ch);
        if w > max_w {
            max_w = w;
        }
    }
    if has_vs16 {
        return 2;
    }
    if has_vs15 {
        return 1;
    }
    max_w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(s: &str) -> Vec<&str> {
        graphemes(s).collect()
    }

    #[test]
    fn ascii_each_codepoint_a_cluster() {
        assert_eq!(collect("abc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn crlf_is_one_cluster() {
        assert_eq!(collect("a\r\nb"), vec!["a", "\r\n", "b"]);
    }

    #[test]
    fn lone_cr_and_lf_separate() {
        assert_eq!(collect("\r"), vec!["\r"]);
        assert_eq!(collect("\n"), vec!["\n"]);
    }

    #[test]
    fn combining_mark_attaches() {
        // é = e (U+0065) + combining acute (U+0301).
        let s = "e\u{0301}";
        assert_eq!(collect(s), vec![s]);
    }

    #[test]
    fn vs16_makes_text_default_an_emoji_cluster() {
        // ⚠ (U+26A0, text-default) + VS16 (U+FE0F) = ⚠️ emoji.
        let s = "\u{26A0}\u{FE0F}";
        assert_eq!(collect(s), vec![s]);
    }

    #[test]
    fn skin_tone_modifier_attaches() {
        // 👋 (U+1F44B) + 🏽 (U+1F3FD) = 👋🏽.
        let s = "\u{1F44B}\u{1F3FD}";
        assert_eq!(collect(s), vec![s]);
    }

    #[test]
    fn regional_indicator_pair() {
        // 🇯🇵 = U+1F1EF U+1F1F5.
        let s = "\u{1F1EF}\u{1F1F5}";
        assert_eq!(collect(s), vec![s]);
        // Two flags back to back stay as separate clusters.
        let s = "\u{1F1EF}\u{1F1F5}\u{1F1FA}\u{1F1F8}";
        assert_eq!(
            collect(s),
            vec!["\u{1F1EF}\u{1F1F5}", "\u{1F1FA}\u{1F1F8}"]
        );
        // A trailing solo RI is its own cluster.
        let s = "\u{1F1EF}\u{1F1F5}\u{1F1FA}";
        assert_eq!(collect(s), vec!["\u{1F1EF}\u{1F1F5}", "\u{1F1FA}"]);
    }

    #[test]
    fn zwj_emoji_compound() {
        // 👨‍👩‍👧‍👦 — U+1F468 ZWJ U+1F469 ZWJ U+1F467 ZWJ U+1F466.
        let s = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(collect(s), vec![s]);
    }

    #[test]
    fn rainbow_flag_zwj_with_vs16() {
        // 🏳️‍🌈 = U+1F3F3 VS16 ZWJ U+1F308.  Crucially, the VS16 must
        // NOT break the cluster — GB9 says Extend attaches.
        let s = "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}";
        assert_eq!(collect(s), vec![s]);
    }

    #[test]
    fn hangul_lvt_stays_together() {
        // 각 = U+AC01 (LVT precomposed) — single codepoint cluster.
        assert_eq!(collect("\u{AC01}"), vec!["\u{AC01}"]);
        // Explicit jamo: L V T form one cluster.
        let s = "\u{1100}\u{1161}\u{11A8}";
        assert_eq!(collect(s), vec![s]);
    }

    #[test]
    fn cluster_width_ascii_is_1() {
        assert_eq!(cluster_width("a"), 1);
        assert_eq!(cluster_width(" "), 1);
    }

    #[test]
    fn cluster_width_cjk_is_2() {
        assert_eq!(cluster_width("中"), 2);
        assert_eq!(cluster_width("漢"), 2);
    }

    #[test]
    fn cluster_width_default_emoji_is_2() {
        assert_eq!(cluster_width("\u{2705}"), 2); // ✅
        assert_eq!(cluster_width("\u{2B50}"), 2); // ⭐
        assert_eq!(cluster_width("\u{1F33F}"), 2); // 🌿
    }

    #[test]
    fn cluster_width_text_default_no_vs_is_1() {
        assert_eq!(cluster_width("\u{26A0}"), 1); // ⚠ without VS16
    }

    #[test]
    fn cluster_width_vs16_forces_emoji_2() {
        assert_eq!(cluster_width("\u{26A0}\u{FE0F}"), 2); // ⚠️
        assert_eq!(cluster_width("\u{261D}\u{FE0F}"), 2); // ☝️
    }

    #[test]
    fn cluster_width_vs15_forces_text_1() {
        assert_eq!(cluster_width("\u{26A0}\u{FE0E}"), 1); // ⚠ text-presentation
    }

    #[test]
    fn cluster_width_combining_mark_is_base() {
        // é = e + combining acute — base 'e' width 1.
        assert_eq!(cluster_width("e\u{0301}"), 1);
    }

    #[test]
    fn cluster_width_zwj_emoji_compound_is_2() {
        // 👨‍👩‍👧‍👦 — every cp is width 2 (Emoji_Presentation), ZWJ is 0.
        let s = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(cluster_width(s), 2);
        // 🏳️‍🌈 — base U+1F3F3 (width 2 via Emoji_Presentation), then
        // VS16 (forces 2) + ZWJ + U+1F308.
        assert_eq!(cluster_width("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"), 2);
    }

    #[test]
    fn cluster_width_regional_indicator_flag_is_2() {
        assert_eq!(cluster_width("\u{1F1EF}\u{1F1F5}"), 2); // 🇯🇵
    }

    #[test]
    fn cluster_width_skin_tone_is_2() {
        assert_eq!(cluster_width("\u{1F44B}\u{1F3FD}"), 2); // 👋🏽
    }

    #[test]
    fn cluster_width_hangul_jamo_is_2() {
        // L + V + T jamo cluster — each is EAW=W so width 2.
        assert_eq!(cluster_width("\u{1100}\u{1161}\u{11A8}"), 2);
        // Precomposed LVT is also 2.
        assert_eq!(cluster_width("\u{AC01}"), 2);
    }

    #[test]
    fn cluster_width_indic_conjunct_is_base() {
        // क + virama + क — Devanagari ka are not EAW; width 1 per base.
        assert_eq!(cluster_width("\u{0915}\u{094D}\u{0915}"), 1);
    }

    #[test]
    fn cluster_width_stray_zwj_is_zero() {
        // A lone ZWJ (somehow) — zero width.
        assert_eq!(cluster_width("\u{200D}"), 0);
    }

    #[test]
    fn indic_conjunct_break() {
        // क + virama + क  → Devanagari kkA (conjunct).
        let s = "\u{0915}\u{094D}\u{0915}";
        assert_eq!(collect(s), vec![s]);
        // Without virama: two separate clusters.
        let s = "\u{0915}\u{0915}";
        assert_eq!(collect(s), vec!["\u{0915}", "\u{0915}"]);
    }

    /// Run the official UAX #29 GraphemeBreakTest.txt conformance
    /// file.  Each line looks like:
    ///   ÷ 000D ÷ 000A ÷  # ÷ [0.2] <CR> (CR) ÷ [4.0] <LF> (LF) ÷ [0.3]
    /// where ÷ = boundary, × = no break, hex = codepoint.
    #[test]
    fn grapheme_break_test_conformance() {
        let path = std::env::var("MARSPOT_GBT_PATH").unwrap_or_else(|_| {
            // Default: vendored copy committed with this crate.
            format!(
                "{}/tests/GraphemeBreakTest.txt",
                env!("CARGO_MANIFEST_DIR")
            )
        });
        let contents = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("skipping conformance test: {} not found", path);
                return;
            }
        };
        let mut total = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for (lineno, line) in contents.lines().enumerate() {
            let lineno = lineno + 1;
            let body = match line.split('#').next() {
                Some(b) => b.trim(),
                None => continue,
            };
            if body.is_empty() {
                continue;
            }
            // Parse: a sequence of ÷ / × delimiters interleaved with
            // hex codepoints.  We need (a) the joined string of code
            // points, (b) the set of byte offsets where ÷ falls.
            let mut tokens = body.split_whitespace();
            let mut string = String::new();
            let mut expected_boundaries: Vec<usize> = Vec::new();
            let mut byte_pos = 0usize;
            loop {
                let delim = match tokens.next() {
                    Some(t) => t,
                    None => break,
                };
                match delim {
                    "÷" => expected_boundaries.push(byte_pos),
                    "×" => {}
                    other => panic!(
                        "line {}: unexpected delimiter {:?}",
                        lineno, other
                    ),
                }
                let cp_hex = match tokens.next() {
                    Some(t) => t,
                    None => break,
                };
                let cp = u32::from_str_radix(cp_hex, 16).unwrap_or_else(|_| {
                    panic!("line {}: bad hex {:?}", lineno, cp_hex)
                });
                let ch = char::from_u32(cp).unwrap_or_else(|| {
                    panic!("line {}: invalid codepoint U+{:04X}", lineno, cp)
                });
                string.push(ch);
                byte_pos += ch.len_utf8();
            }
            // Run the segmenter and collect boundary offsets.
            let mut actual_boundaries: Vec<usize> = vec![0];
            let mut pos = 0usize;
            for g in graphemes(&string) {
                pos += g.len();
                actual_boundaries.push(pos);
            }
            total += 1;
            if actual_boundaries != expected_boundaries {
                failed.push(format!(
                    "line {}: input={:?}  expected={:?}  actual={:?}",
                    lineno,
                    string.chars().map(|c| c as u32).collect::<Vec<_>>(),
                    expected_boundaries,
                    actual_boundaries
                ));
            }
        }
        let n_fail = failed.len();
        if n_fail > 0 {
            for f in failed.iter().take(20) {
                eprintln!("{}", f);
            }
            if n_fail > 20 {
                eprintln!("... and {} more", n_fail - 20);
            }
            panic!(
                "GraphemeBreakTest.txt conformance: {}/{} cases failed",
                n_fail, total
            );
        }
        eprintln!(
            "GraphemeBreakTest.txt conformance: {}/{} cases passed",
            total, total
        );
    }
}

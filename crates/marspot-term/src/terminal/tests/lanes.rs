//! The fast lanes against the state machine: width classes, batched runs,
//! and the ascii fast path matching the slow path.

use super::*;

/// The fast pictograph class is allowed to commit a character as
/// a cluster of its own without asking the segmenter.  That is
/// only sound while every character it admits is (a) GBP=Other,
/// so no neighbour can join across the boundary, and (b) two
/// cells wide by the same table `char_width` reads — otherwise
/// the fast and slow paths would disagree about layout.
///
/// Walks the entire codepoint space rather than a sample: the
/// class is defined by range exclusions, and the thing that
/// would break it is a Unicode update adding a character the
/// ranges do not cover.  A sample of today's emoji cannot see
/// that; this can.
#[test]
fn the_fast_pictograph_class_never_outruns_the_tables() {
    use crate::unicode_data::{GBP, gbp};
    for cp in 0..0x11_0000u32 {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        let Some(w) = super::fast_pict_width(ch) else {
            continue;
        };
        assert_eq!(
            gbp(cp),
            GBP::Other,
            "U+{cp:04X} admitted to the fast class but joins its neighbours"
        );
        assert!(
            crate::emoji_presentation::has_emoji_presentation(cp),
            "U+{cp:04X} admitted without emoji presentation"
        );
        assert_eq!(w, 2, "U+{cp:04X} fast width disagrees with itself");
        assert_eq!(
            crate::grid::char_width(ch),
            2,
            "U+{cp:04X} fast width disagrees with char_width"
        );
    }
}

/// Pins the fast path's hardcoded class facts against the real
/// UCD tables: EVERY codepoint `boring_width` claims must be
/// GBP::Other, not Extended_Pictographic, InCB::None, and have
/// exactly the claimed single-char cluster_width.  Walking the
/// whole BMP+ExtA space keeps the range list itself honest — a
/// future `bin/regen-unicode-tables.sh` regen that invalidated a
/// member fails here before the fast path can mis-render.
#[test]
fn fast_path_class_is_sound() {
    use crate::unicode_data::{GBP, InCB, gbp, incb, is_extended_pictographic};
    for cp in 0x20u32..=0xFFFF {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        let Some(w) = boring_width(ch) else { continue };
        assert_eq!(gbp(cp), GBP::Other, "U+{cp:04X} gbp");
        assert!(!is_extended_pictographic(cp), "U+{cp:04X} pictographic");
        assert_eq!(incb(cp), InCB::None, "U+{cp:04X} incb");
        assert_eq!(
            crate::grapheme::cluster_width(&ch.to_string()),
            w,
            "U+{cp:04X} width"
        );
    }
    // Hangul syllables ride the wider `fast_width` class: LV/LVT
    // only, never pictographic / InCB, always 2 cells.
    for cp in 0xAC00u32..=0xD7A3 {
        let ch = char::from_u32(cp).unwrap();
        assert_eq!(fast_width(ch), Some(2), "U+{cp:04X} fast_width");
        assert!(
            matches!(gbp(cp), GBP::LV | GBP::LVT),
            "U+{cp:04X} gbp must be LV/LVT"
        );
        assert!(!is_extended_pictographic(cp), "U+{cp:04X} pictographic");
        assert_eq!(incb(cp), InCB::None, "U+{cp:04X} incb");
        assert_eq!(
            crate::grapheme::cluster_width(&ch.to_string()),
            2,
            "U+{cp:04X} width"
        );
    }
    // Spot-check deliberate exclusions.
    assert!(boring_width('\u{1F}').is_none()); // C0
    assert!(boring_width('\u{7F}').is_none()); // DEL
    assert!(boring_width('é').is_none()); // Latin-1 (can NFD-combine)
    assert!(boring_width('\u{3099}').is_none()); // combining kana mark
    assert!(boring_width('\u{AC00}').is_none()); // Hangul stays out of boring
    assert!(boring_width('⭐').is_none()); // pictographic
    assert!(fast_width('\u{1100}').is_none()); // L jamo — GB6 joins forward
    assert!(fast_width('\u{11A8}').is_none()); // T jamo — GB8 joins backward
}

/// Hangul through the fast/batch lanes: precomposed syllables
/// batch as width-2 cells; conjoining jamo still fuse via the
/// slow path (the last run char stays buffered).
#[test]
fn hangul_fast_lane_semantics() {
    // 안녕하세요 — five LVT/LV syllables, 2 cells each.
    let t = term_with(20, 4, "안녕하세요".as_bytes());
    for (i, ch) in "안녕하세요".chars().enumerate() {
        assert_eq!(t.grid().cell(i as u16 * 2, 0).ch, ch);
        assert_eq!(t.grid().cell(i as u16 * 2 + 1, 0).ch, '\0');
    }
    assert_eq!(t.grid().cursor(), (10, 0));

    // LV syllable + trailing T jamo conjoin into ONE cluster
    // (GB8 via the buffered last char), not two cell pairs.
    let t = term_with(20, 4, "\u{AC00}\u{11A8}z".as_bytes());
    let syllable = t.grid().cell(0, 0);
    assert_eq!(t.grid().cluster_text(&syllable), Some("\u{AC00}\u{11A8}"));
    assert_eq!(t.grid().cell(2, 0).ch, 'z');

    // L + V jamo compose via the slow path (neither is fast class).
    let t = term_with(20, 4, "\u{1100}\u{1161}z".as_bytes());
    let composed = t.grid().cell(0, 0);
    assert_eq!(t.grid().cluster_text(&composed), Some("\u{1100}\u{1161}"));
    assert_eq!(t.grid().cell(2, 0).ch, 'z');
}

/// GB9b regression: a pending cluster ending in a Prepend char
/// absorbs the following printable — the batch lanes must detect
/// that and fall back instead of flushing early.
#[test]
fn prepend_cluster_absorbs_batch_head() {
    // U+0600 ARABIC NUMBER SIGN (GBP Prepend) + "1234":
    // old scalar semantics = cluster [0600, '1'] commits base
    // U+0600 at cluster_width, then '2','3','4' follow.
    let w = crate::grapheme::cluster_width("\u{0600}1") as u16;
    let t = term_with(20, 4, "\u{0600}1234".as_bytes());
    let prepended = t.grid().cell(0, 0);
    assert_eq!(t.grid().cluster_text(&prepended), Some("\u{0600}1"));
    assert_eq!(t.grid().cell(w, 0).ch, '2');
    assert_eq!(t.grid().cell(w + 1, 0).ch, '3');
    assert_eq!(t.grid().cell(w + 2, 0).ch, '4');
}

/// Batch-lane specifics: right-edge wrap semantics must chain
/// exactly like repeated single-width `write_glyph` calls.
#[test]
fn batch_ascii_run_wrap_semantics() {
    // 10-col grid, 25-char run → rows fill + wrapped flags.
    let t = term_with(10, 4, b"abcdefghijklmnopqrstuvwxy");
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    assert_eq!(t.grid().cell(9, 0).ch, 'j');
    assert_eq!(t.grid().cell(0, 1).ch, 'k');
    assert_eq!(t.grid().cell(9, 1).ch, 't');
    assert_eq!(t.grid().cell(0, 2).ch, 'u');
    assert_eq!(t.grid().cell(4, 2).ch, 'y');
    assert!(t.grid().row_wrapped(1));
    assert!(t.grid().row_wrapped(2));
    assert_eq!(t.grid().cursor(), (5, 2));

    // Run ending EXACTLY at the right edge defers the wrap: cursor
    // parks on the last column, next glyph wraps, a CR instead
    // cancels without advancing (classic DECAWM).
    let t = term_with(10, 4, b"0123456789");
    assert_eq!(t.grid().cursor(), (9, 0));
    let t = term_with(10, 4, b"0123456789X");
    assert_eq!(t.grid().cell(0, 1).ch, 'X');
    let t = term_with(10, 4, b"0123456789\r\nY");
    assert_eq!(t.grid().cell(0, 1).ch, 'Y');
    assert_eq!(t.grid().cell(9, 0).ch, '9');

    // Wide cluster immediately before a batch run: run starts
    // after the trail pad.
    let t = term_with(20, 4, "⭐abcdef".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, '⭐');
    assert_eq!(t.grid().cell(2, 0).ch, 'a');
    assert_eq!(t.grid().cell(7, 0).ch, 'f');

    // SGR mid-stream splits the run at the escape; attrs apply to
    // the following batch.
    let t = term_with(20, 4, b"ab\x1b[1mcd");
    assert_eq!(t.grid().cell(2, 0).ch, 'c');
    assert!(t.grid().cell(2, 0).attrs.bold);
    assert!(!t.grid().cell(1, 0).attrs.bold);
}

/// Wide batch lane: CJK runs commit through `print_wide_run` with
/// the same wrap semantics as repeated `write_glyph(_, 2)`.
#[test]
fn batch_wide_run_wrap_semantics() {
    // 10-col grid: 4 CJK chars/row, 9 chars → 3 rows (8 cells +
    // 2-cell tail per row boundary).
    let t = term_with(10, 4, "一二三四五六七八九".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, '一');
    assert_eq!(t.grid().cell(8, 0).ch, '五');
    assert_eq!(t.grid().cell(9, 0).ch, '\0'); // trail pad
    assert_eq!(t.grid().cell(0, 1).ch, '六');
    assert!(t.grid().row_wrapped(1));
    assert_eq!(t.grid().cell(6, 1).ch, '九');
    assert_eq!(t.grid().cursor(), (8, 1));

    // Odd column start: ASCII then CJK — last column unusable,
    // NUL pad + wrap, identical to the scalar wide path.
    let t = term_with(5, 4, "ab一二".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    assert_eq!(t.grid().cell(2, 0).ch, '一');
    assert_eq!(t.grid().cell(4, 0).ch, '\0'); // unusable last col pad
    assert_eq!(t.grid().cell(0, 1).ch, '二');
    assert!(t.grid().row_wrapped(1));

    // Run ends exactly at the right edge → deferred wrap.
    let t = term_with(4, 4, "一二".as_bytes());
    assert_eq!(t.grid().cursor(), (3, 0));
    let t = term_with(4, 4, "一二三".as_bytes());
    assert_eq!(t.grid().cell(0, 1).ch, '三');

    // CJK then combining mark: the last run char stays the open
    // cluster and the mark joins it, without a stray cell.  The
    // mark used to be dropped here; the cell now holds the whole
    // cluster and `ch` is the pool index that says so.
    let t = term_with(20, 4, "水木\u{3099}z".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, '水');
    let joined = t.grid().cell(2, 0);
    assert_eq!(
        t.grid().cluster_text(&joined),
        Some("木\u{3099}"),
        "the mark used to be dropped"
    );
    assert_eq!(t.grid().cell(4, 0).ch, 'z');
}

/// The fast path must be behaviourally invisible: interleaving
/// ASCII with cluster-forming codepoints in every adjacency order
/// yields the same grid as the pure-slow-path semantics.
#[test]
fn ascii_fast_path_matches_slow_path_semantics() {
    // ASCII then combining mark (fast → slow transition): the
    // mark arrives with an unsynced segmenter and must not fuse
    // wrongly or emit a stray cell.
    let t = term_with(20, 4, "ae\u{0301}b".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    let joined = t.grid().cell(1, 0);
    assert_eq!(
        t.grid().cluster_text(&joined),
        Some("e\u{0301}"),
        "the mark used to be dropped here"
    );
    assert_eq!(t.grid().cell(2, 0).ch, 'b');
    assert_eq!(t.grid().cursor(), (3, 0));

    // Wide cluster then ASCII (slow → fast transition).
    let t = term_with(20, 4, "⭐x".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, '⭐');
    assert_eq!(t.grid().cell(2, 0).ch, 'x');

    // ASCII split across feeds — end-of-feed flush + fresh feed.
    let mut t = Terminal::new(20, 4);
    t.feed(b"ab");
    t.feed(b"cd");
    for (i, ch) in ['a', 'b', 'c', 'd'].into_iter().enumerate() {
        assert_eq!(t.grid().cell(i as u16, 0).ch, ch);
    }

    // ASCII at end of feed, combining mark opens the next feed:
    // same visible result as the single-feed case above.
    let mut t = Terminal::new(20, 4);
    t.feed(b"e");
    t.feed("\u{0301}z".as_bytes());
    let joined = t.grid().cell(0, 0);
    assert_eq!(t.grid().cluster_text(&joined), Some("e\u{0301}"));
    assert_eq!(t.grid().cell(1, 0).ch, 'z');

    // VS16 emoji sandwiched in ASCII keeps its 2-cell width.
    let t = term_with(20, 4, "a\u{26A0}\u{FE0F}b".as_bytes());
    assert_eq!(t.grid().cell(0, 0).ch, 'a');
    let vs16 = t.grid().cell(1, 0);
    assert_eq!(t.grid().cluster_text(&vs16), Some("\u{26A0}\u{FE0F}"));
    assert_eq!(t.grid().cell(2, 0).ch, '\0'); // wide trail pad
    assert_eq!(t.grid().cell(3, 0).ch, 'b');
}

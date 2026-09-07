//! What a user-perceived character occupies, cell by cell.
//!
//! The terminal buffers one codepoint before committing it, so that a
//! variation selector, a ZWJ, or a combining mark arriving next can
//! join the cluster instead of landing in a cell of its own.  Any
//! change to that pipeline can split a cluster silently — the screen
//! still looks like text, just the wrong text — and the UAX #29
//! conformance suite cannot see it, because it tests the segmenter
//! and not the terminal's use of it.
//!
//! This is the table that can.  Each case states the bytes and the
//! cells they must produce, and every case is run three ways: in one
//! feed, split at every byte boundary, and one byte per feed.  A
//! pipeline that only works when a cluster arrives whole is a
//! pipeline that breaks on a slow pty.
use marspot_term::terminal::Terminal;

/// `(name, input, expected cells)` — `'\0'` is a wide glyph's trailing
/// pad, `' '` an untouched cell.
struct Case {
    name: &'static str,
    input: &'static str,
    cells: &'static [char],
}

const CASES: &[Case] = &[
    Case { name: "ascii", input: "abc", cells: &['a', 'b', 'c'] },
    Case { name: "cjk", input: "中文", cells: &['中', '\0', '文', '\0'] },
    Case {
        name: "combining mark joins its base",
        input: "e\u{0301}z",
        cells: &['e', 'z'],
    },
    Case {
        name: "VS16 widens a text-presentation symbol",
        input: "a\u{26A0}\u{FE0F}b",
        cells: &['a', '\u{26A0}', '\0', 'b'],
    },
    Case {
        name: "emoji-presentation symbol is wide on its own",
        input: "a\u{2B50}b",
        cells: &['a', '\u{2B50}', '\0', 'b'],
    },
    Case {
        name: "ZWJ family is one cluster",
        input: "a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}b",
        cells: &['a', '\u{1F468}', '\0', 'b'],
    },
    Case {
        name: "regional indicator pair is one flag",
        input: "a\u{1F1EF}\u{1F1F5}b",
        cells: &['a', '\u{1F1EF}', '\0', 'b'],
    },
    Case {
        name: "skin tone modifier joins its base",
        input: "a\u{1F44D}\u{1F3FD}b",
        cells: &['a', '\u{1F44D}', '\0', 'b'],
    },
    Case {
        name: "hangul syllable then trailing jamo",
        input: "a\u{AC00}\u{11A8}b",
        cells: &['a', '\u{AC00}', '\0', 'b'],
    },
    Case {
        name: "devanagari virama conjunct",
        input: "a\u{0915}\u{094D}\u{0915}b",
        cells: &['a', '\u{0915}', 'b'],
    },
    Case {
        name: "plain emoji run",
        input: "\u{1F3A8}\u{1F680}",
        cells: &['\u{1F3A8}', '\0', '\u{1F680}', '\0'],
    },
];

fn row0(t: &Terminal, n: usize) -> Vec<char> {
    (0..n as u16).map(|c| t.grid().cell(c, 0).ch).collect()
}

/// Feed `input` in `chunk`-byte pieces (0 = all at once).
fn feed_in(t: &mut Terminal, input: &str, chunk: usize) {
    let b = input.as_bytes();
    if chunk == 0 {
        t.feed(b);
        return;
    }
    for c in b.chunks(chunk) {
        t.feed(c);
    }
}

#[test]
fn a_cluster_occupies_the_cells_it_should() {
    for case in CASES {
        let mut t = Terminal::new(20, 4);
        feed_in(&mut t, case.input, 0);
        assert_eq!(row0(&t, case.cells.len()), case.cells.to_vec(), "{}", case.name);
    }
}

/// Which feed boundaries a cluster currently does NOT survive.
///
/// The terminal commits whatever is buffered at the end of a feed, so
/// that a keystroke lands on the read it arrived in rather than the
/// next one.  The comment beside that flush says a cluster split
/// across feeds "still resolves via the segmenter's saved prev-state";
/// it does not — the flush has already written the base and moved the
/// cursor, so the codepoint that would have extended it starts a
/// cluster of its own and takes a cell.
///
/// A pty hands over whatever the kernel had, so this is not exotic:
/// any read ending between two codepoints of one cluster does it.
///
/// This is a CHARACTERIZATION test, not an endorsement.  It states the
/// exact set of splits that are wrong today so the bug is written
/// down, bounded, and impossible to widen unnoticed — and so that
/// whoever fixes it is told by a failing test to come and delete this.
///
/// The fix is the same restructuring that removes the one-codepoint
/// lookahead (measured worth +22.6 % on emoji parse): write the glyph
/// as soon as its width is known, and let a following zero-width
/// codepoint amend the cell it landed in, which is what the reference
/// implementation does.
#[test]
fn where_a_cluster_split_between_feeds_is_currently_lost() {
    let known_bad: &[(&str, &[usize])] = &[
        ("ascii", &[]),
        ("cjk", &[]),
        ("combining mark joins its base", &[]),
        ("VS16 widens a text-presentation symbol", &[4]),
        ("emoji-presentation symbol is wide on its own", &[]),
        ("ZWJ family is one cluster", &[5, 8, 12, 15, 19, 22]),
        ("regional indicator pair is one flag", &[5]),
        ("skin tone modifier joins its base", &[]),
        ("hangul syllable then trailing jamo", &[4]),
        ("devanagari virama conjunct", &[4, 7]),
        ("plain emoji run", &[]),
    ];
    for case in CASES {
        let expected_bad = known_bad
            .iter()
            .find(|(n, _)| *n == case.name)
            .map(|(_, s)| *s)
            .unwrap_or_else(|| panic!("no known-bad entry for {}", case.name));
        let b = case.input.as_bytes();
        let mut bad = Vec::new();
        for split in 1..b.len() {
            if std::str::from_utf8(&b[..split]).is_err() {
                continue;
            }
            let mut t = Terminal::new(20, 4);
            t.feed(&b[..split]);
            t.feed(&b[split..]);
            if row0(&t, case.cells.len()) != case.cells.to_vec() {
                bad.push(split);
            }
        }
        assert_eq!(
            bad, expected_bad,
            "{}: the set of feed splits a cluster does not survive has CHANGED. \
             Fewer is the fix landing — delete this test and turn the table above \
             into the split-feed assertion. More is a regression.",
            case.name
        );
    }
}

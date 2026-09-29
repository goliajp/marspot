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

/// A cluster survives being split between two reads.
///
/// A pty ends its reads wherever the kernel had a break, so a cluster
/// arrives with its base in one and its modifier in the next as a
/// matter of course.  This used to come apart: the terminal committed
/// whatever was buffered at the end of a feed, so the base was drawn
/// and the cursor moved before the codepoint that would have extended
/// it arrived — `a⚠️b` split after `⚠` put the VS16 in a cell of its
/// own (measured 2026-09-07, along with the ZWJ family, the flag, the
/// hangul syllable and the virama conjunct).
///
/// A glyph is now committed as soon as its width is known and a later
/// codepoint amends the cell it landed in, so there is nothing to
/// flush at a feed boundary and nothing to lose.
#[test]
fn a_cluster_survives_being_split_between_reads() {
    for case in CASES {
        let b = case.input.as_bytes();
        for split in 1..b.len() {
            if std::str::from_utf8(&b[..split]).is_err() {
                continue;
            }
            let mut t = Terminal::new(20, 4);
            t.feed(&b[..split]);
            t.feed(&b[split..]);
            assert_eq!(
                row0(&t, case.cells.len()),
                case.cells.to_vec(),
                "{} split at byte {split}",
                case.name
            );
        }
    }
}

/// And in whatever pieces the reads happen to come in.
#[test]
fn a_cluster_survives_any_chunking() {
    for case in CASES {
        for chunk in [1usize, 2, 3, 5] {
            let mut t = Terminal::new(20, 4);
            feed_in(&mut t, case.input, chunk);
            assert_eq!(
                row0(&t, case.cells.len()),
                case.cells.to_vec(),
                "{} in {chunk}-byte chunks",
                case.name
            );
        }
    }
}

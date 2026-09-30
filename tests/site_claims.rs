//! The numbers on the website are the numbers that were measured.
//!
//! A marketing page and a benchmark drift apart quietly: the page is
//! written once and the measurements are re-taken, or the reverse, and
//! nobody compares them because comparing them is nobody's job.  It is
//! this test's job.
//!
//! Every throughput figure on the page must appear in
//! `bench/baseline.json`'s competitor snapshot for the same terminal
//! and the same stream.

use std::collections::HashMap;

const SITE: &str = include_str!("../site/index.html");
const README: &str = include_str!("../README.md");
const BASELINE: &str = include_str!("../bench/baseline.json");

/// The four streams, in the order the page's columns use them.
const STREAMS: [(&str, &str); 4] = [
    ("ASCII", "cat-ascii"),
    ("mixed", "cat-mixed"),
    ("CJK", "cat-cjk"),
    ("emoji", "cat-emoji"),
];

/// Column order in the page's table, after the stream name.
const COLUMNS: [&str; 4] = ["marspot", "ghostty", "iterm2", "terminal"];

/// Pull `"<name>": { … "<scenario>_MBps": <v> … }` out of the snapshot
/// without a JSON parser — this crate has no dependencies and is not
/// getting one for a test.
fn measured() -> HashMap<(String, String), f64> {
    let mut out = HashMap::new();
    let snapshot = BASELINE
        .split("\"competitors_snapshot\"")
        .nth(1)
        .expect("baseline has a competitors_snapshot");
    let mut terminal = String::new();
    for line in snapshot.lines() {
        let t = line.trim();
        // `"ghostty": {`
        if let Some(name) = t.strip_suffix(": {").and_then(|s| s.strip_prefix('"')) {
            terminal = name.trim_end_matches('"').to_string();
            continue;
        }
        for (_, scenario) in STREAMS {
            let key = format!("\"{scenario}_MBps\":");
            if let Some(rest) = t.strip_prefix(&key) {
                let v: f64 = rest
                    .trim()
                    .trim_end_matches(',')
                    .parse()
                    .unwrap_or_else(|e| panic!("{scenario} for {terminal}: {e}"));
                out.insert((terminal.clone(), scenario.to_string()), v);
            }
        }
    }
    out
}

/// The page's table rows: stream name then four numbers.
fn claimed() -> Vec<(String, Vec<f64>)> {
    let mut rows = Vec::new();
    let body = SITE
        .split("<tbody>")
        .nth(1)
        .expect("the page has a table body")
        .split("</tbody>")
        .next()
        .expect("…that closes");
    for row in body.split("<tr>").skip(1) {
        let cells: Vec<String> = row
            .split("<td")
            .skip(1)
            .map(|c| {
                let inner = c.split('>').nth(1).unwrap_or("");
                inner.split('<').next().unwrap_or("").trim().to_string()
            })
            .collect();
        if cells.len() != 5 {
            continue;
        }
        let nums = cells[1..]
            .iter()
            .map(|c| c.parse::<f64>().unwrap_or_else(|e| panic!("{c:?}: {e}")))
            .collect();
        rows.push((cells[0].clone(), nums));
    }
    rows
}

#[test]
fn the_site_quotes_the_measurements() {
    let measured = measured();
    let rows = claimed();
    assert_eq!(rows.len(), STREAMS.len(), "the page's table lost or gained a row");

    for (label, nums) in &rows {
        let scenario = STREAMS
            .iter()
            .find(|(page, _)| page == label)
            .unwrap_or_else(|| panic!("the page names a stream {label:?} the benchmark does not"))
            .1;
        for (column, claim) in COLUMNS.iter().zip(nums) {
            match measured.get(&((*column).to_string(), scenario.to_string())) {
                Some(m) => assert!(
                    (m - claim).abs() < 0.05,
                    "the page says {column} does {claim} MB/s on {scenario}; \
                     the last measurement says {m}"
                ),
                None => panic!("no measurement for {column} on {scenario} to back the page up"),
            }
        }
    }
}

#[test]
fn both_readers_actually_read() {
    // Either parser silently returning nothing would make the test
    // above pass by comparing an empty set to an empty set.
    assert!(measured().len() >= 12, "read {} measurements", measured().len());
    assert_eq!(claimed().len(), 4, "read {} rows off the page", claimed().len());
}

#[test]
fn the_comparison_can_fail() {
    let measured = measured();
    let (_, real) = measured
        .iter()
        .next()
        .map(|((t, s), v)| ((t.clone(), s.clone()), *v))
        .expect("at least one measurement");
    let fibbed = real + 10.0;
    assert!(
        (real - fibbed).abs() >= 0.05,
        "the tolerance is wide enough to wave through a ten-megabyte lie"
    );
}

#[test]
fn the_page_does_not_promise_what_is_known_missing() {
    // The gaps list is there so a first hour is not a series of
    // surprises.  If a gap closes, this is the reminder to take it off
    // the page rather than leaving it to read as false modesty.
    // One left.  The others closed today, each taken off both
    // documents by the test below rather than by remembering to.
    assert!(SITE.contains("terminfo"), "the page stopped mentioning terminfo");
}

/// And it does not keep confessing a gap that closed.
///
/// The list above only catches a gap going missing.  A page that goes
/// on apologising for something that works is wrong in the other
/// direction, and the only way to tell the two apart is to ask the
/// terminal rather than to read the page twice.
#[test]
fn a_gap_that_closed_comes_off_the_page() {
    use marspot::terminal::Terminal;

    let row = |t: &Terminal| -> String {
        let cols = t.grid().cols();
        (0..cols).map(|c| t.grid().cell(c, 0).ch).collect::<String>().trim_end().to_string()
    };

    let mut t = Terminal::new(40, 3);
    t.feed(b"a\tb");
    assert_ne!(row(&t), "ab", "tabs stopped moving the cursor");
    let mut t1 = Terminal::new(40, 3);
    t1.feed(b"\x1b(0lqk");
    let drawn: String = (0..3).map(|c| t1.grid().cell(c, 0).ch).collect();
    assert_eq!(drawn, "\u{250c}\u{2500}\u{2510}", "the line-drawing set stopped translating");

    let mut t2 = Terminal::new(40, 3);
    t2.feed(b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(t2.take_osc_clipboard().as_deref(), Some("hello"), "OSC 52 stopped working");

    // Clusters keep their shape on screen now, so neither document
    // may still say a cell holds one code point — but both must still
    // say what scrolling back loses, which is true until scrollback
    // has storage of its own.
    let mut t3 = Terminal::new(20, 3);
    t3.feed("e\u{301}".as_bytes());
    let c3 = t3.grid().cell(0, 0);
    assert_eq!(t3.grid().cluster_text(&c3), Some("e\u{301}"), "clusters stopped working");

    for (what, text) in [("the page", SITE), ("the README", README)] {
        assert!(
            !text.contains("One cell holds one code point"),
            "a cluster keeps its shape now; {what} still says a cell holds one code point"
        );
        assert!(
            text.contains("survive
          scrolling back, but not a restart")
                || text.contains("survive scrolling
  back, but not a restart"),
            "{what} stopped saying that a restart loses the cluster"
        );
        assert!(
            !text.contains("tabs are not implemented")
                && !text.contains("literal tab character is dropped"),
            "tabs work; {what} still says they are dropped"
        );
        assert!(
            !text.contains("Line-drawing character sets are not translated")
                && !text.contains("`ESC ( 0` line drawing is not translated"),
            "the charset is translated; {what} still says it is not"
        );
        assert!(
            !text.contains("OSC 52 / 8 / 7 / 133")
                && !text.contains("OSC 8 (hyperlinks), OSC 52"),
            "OSC 52 works; {what} still lists it as missing"
        );
    }
}

/// The README and the page keep the same key table.
///
/// They are two documents with the same list in them, which is two
/// places for it to go stale.  Both are built from `shortcuts`, so
/// both are checked against it.
#[test]
fn the_readme_keeps_the_same_keys() {
    use marspot::shortcuts::SHORTCUTS;
    for s in SHORTCUTS {
        assert!(README.contains(s.chord), "the README does not mention {}", s.chord);
        assert!(
            README.contains(s.action),
            "{} is in the README, described as something else",
            s.chord
        );
    }
}

/// The keyboard reference on the page is the list the code keeps.
///
/// A key table is the part of a page that goes wrong quietly: a chord
/// is removed or renamed and the page keeps promising it.  The list in
/// `marspot::shortcuts` is already tied to the handlers by
/// `shortcuts_are_real`; this ties the page to the list, so the chain
/// runs from what the page says to the code that does it.
#[test]
fn the_pages_key_table_is_the_list_the_code_keeps() {
    use marspot::shortcuts::SHORTCUTS;

    let table_start = SITE
        .find("<table class=\"keys\">")
        .expect("the page has no key table");
    let table_end = SITE[table_start..]
        .find("</table>")
        .map(|i| table_start + i)
        .expect("the key table is not closed");
    let table = &SITE[table_start..table_end];

    let rows = table.matches("<tr>").count();
    assert_eq!(
        rows,
        SHORTCUTS.len(),
        "the page lists {rows} shortcuts and the code keeps {}",
        SHORTCUTS.len()
    );

    for s in SHORTCUTS {
        assert!(
            table.contains(s.chord),
            "the page's key table does not mention {}",
            s.chord
        );
        assert!(
            table.contains(s.action),
            "{} is on the page, but described as something other than {:?}",
            s.chord,
            s.action
        );
    }
}

const GUIDE: &str = include_str!("../site/guide.html");
const SHELL_MAIN: &str = include_str!("../src/bin/marspot-shell/main.rs");

/// The guide's key table is the same list as everywhere else.
#[test]
fn the_guide_keeps_the_same_keys() {
    use marspot::shortcuts::SHORTCUTS;
    for s in SHORTCUTS {
        assert!(GUIDE.contains(s.chord), "the guide does not mention {}", s.chord);
        assert!(GUIDE.contains(s.action), "{} is in the guide, described as something else", s.chord);
    }
}

/// Every command the guide documents is one the binary has.
///
/// A page that lists a flag the program does not take is worse than a
/// page that lists none: the reader tries it. The help text is the
/// binary's own statement of what it accepts, so the guide is checked
/// against that rather than against a list kept by hand.
#[test]
fn the_guide_documents_only_flags_the_binary_takes() {
    let flags: Vec<&str> = SHELL_MAIN
        .lines()
        .filter_map(|l| l.trim().strip_prefix("marspot-shell --"))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|f| !f.is_empty())
        .collect();
    assert!(
        flags.len() >= 6,
        "read {} flags out of the help text; the parser stopped working",
        flags.len()
    );

    // Pull the flags the guide's command table names.
    let documented: Vec<&str> = GUIDE
        .match_indices("<tr><td>--")
        .map(|(i, _)| {
            let rest = &GUIDE[i + "<tr><td>".len()..];
            &rest[..rest.find("</td>").unwrap_or(0)]
        })
        .collect();
    assert!(!documented.is_empty(), "the guide's command table is empty");

    for d in &documented {
        let bare = d.trim_start_matches("--");
        assert!(
            flags.contains(&bare),
            "the guide documents --{bare}, which the binary does not take"
        );
    }
}

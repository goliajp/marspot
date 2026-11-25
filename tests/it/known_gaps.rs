//! Every entry under the README's "Known gaps" is pinned here by a test
//! that asks the code, not the text: the gap must still be there for
//! the README (and the site page, which repeats most of the list) to
//! keep saying so.  When one of these fails because a gap closed, the
//! entry comes off the README and, where it repeats it, off the page.
//!
//! `the_list_is_the_list_the_tests_know` keeps the two in step the
//! other way: an entry added without a test here fails it.

use marspot::terminal::Terminal;

mod links;
mod terminfo;

const SITE: &str = include_str!("../../site/index.html");
const README: &str = include_str!("../../README.md");

/// A key phrase from each entry, in the README's order, and the test
/// that pins it.
const ENTRIES: [(&str, &str); 5] = [
    ("OSC 8 links in history kept on disk", "links::*"),
    (
        "OSC 8 links open only",
        "links::osc8_opens_only_four_schemes",
    ),
    ("No OSC 7", "osc_7_none_133_no_jump_key_52_write_only"),
    ("There is no terminfo entry of its own", "terminfo::*"),
    (
        "The settings panel offers the theme",
        "fonts_font_size_and_keys_are_not_settings",
    ),
];

/// The README's section, so a phrase elsewhere does not count.
fn gaps() -> &'static str {
    let from = README
        .find("## Known gaps")
        .expect("the README has a Known gaps section");
    let rest = &README[from..];
    let to = rest[3..].find("\n## ").map(|i| i + 3).unwrap_or(rest.len());
    &rest[..to]
}

fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn says(phrase: &str) {
    assert!(
        flat(gaps()).contains(phrase),
        "Known gaps no longer says {phrase:?}"
    );
}

fn the_page_says(phrase: &str) {
    assert!(
        flat(SITE).contains(phrase),
        "the page no longer says {phrase:?}"
    );
}

/// Every Rust source outside the terminal engine, as (path, text).
fn app_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                if !path.ends_with("marspot-term") && !path.ends_with("target") {
                    walk(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("read a source file");
                out.push((path.display().to_string(), text));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&root.join("src"), &mut out);
    walk(&root.join("crates"), &mut out);
    assert!(out.len() > 100, "read only {} source files", out.len());
    out
}

/// The message for a gap that closed.
fn closed(what: &str) -> String {
    format!(
        "{what}: the gap closed, so take its entry off README.md's Known gaps \
         (and off site/index.html's \"Not yet\" list where it repeats it), \
         then drop or invert this check"
    )
}

#[test]
fn the_list_is_the_list_the_tests_know() {
    let bullets: Vec<String> = gaps()
        .split("\n- ")
        .skip(1)
        .map(|b| flat(b.split("\n\n").next().unwrap()))
        .collect();
    assert_eq!(
        bullets.len(),
        ENTRIES.len(),
        "Known gaps has {} entries and tests/it/known_gaps.rs pins {}; an entry \
         needs a test here that fails when the gap closes: {bullets:#?}",
        bullets.len(),
        ENTRIES.len()
    );
    for (bullet, (phrase, test)) in bullets.iter().zip(ENTRIES) {
        assert!(
            bullet.starts_with(phrase),
            "Known gaps entry {bullet:?} is not the one {test} pins ({phrase:?})"
        );
    }
    for (what, text) in [("README.md", README), ("site/index.html", SITE)] {
        let text = text.to_lowercase();
        assert!(
            !text.contains("not yet packaged") && !text.contains("not packaged yet"),
            "{what} still says the app is not yet packaged"
        );
    }
}

/// OSC 7 is not read, OSC 133 marks are kept but no command jumps to
/// one, and OSC 52 is written but never read back.
#[test]
fn osc_7_none_133_no_jump_key_52_write_only() {
    says("No OSC 7 (working directory");
    says("OSC 133 prompt marks are read and kept with the history, but nothing uses them yet");
    says("there is no key to jump to the previous command");
    says("reading it back is refused on purpose");
    the_page_says("No working-directory escape sequence");
    the_page_says("are read and kept with the history, but nothing uses them yet");
    the_page_says("reading it back is refused");

    // OSC 7: the dispatcher has an arm for 8 and 133 and none for 7
    const OSC: &str = include_str!("../../crates/marspot-term/src/terminal/osc.rs");
    let dispatch = OSC
        .split("fn osc(")
        .nth(1)
        .expect("terminal/osc.rs has the dispatcher")
        .split("\n    fn ")
        .next()
        .unwrap();
    assert!(
        dispatch.contains("Some(133)") && dispatch.contains("Some(8)"),
        "the probe no longer reads the dispatcher"
    );
    for n in ["Some(7)", "Some(7 |", "| 7)"] {
        assert!(
            !dispatch.contains(n),
            "{}",
            closed(&format!("the OSC dispatcher handles {n}"))
        );
    }
    // and nothing is kept, answered or landed anywhere else
    // (one feed each: the snapshot carries a count of feeds)
    let mut without = Terminal::new(40, 3);
    without.feed(b"$ ");
    let mut t = Terminal::new(40, 3);
    t.feed(b"$ \x1b]7;file://host/tmp\x07");
    assert!(t.take_response().is_empty());
    assert_eq!(t.osc_title(), "", "OSC 7 landed in the title");
    assert_eq!(
        t.serialize_snapshot_live(),
        without.serialize_snapshot_live(),
        "{}",
        closed("OSC 7 changed the terminal's state")
    );

    // OSC 133: the mark is on the row, and still there once the row
    // has scrolled into history
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b]133;A\x07$ ");
    assert_eq!(
        t.grid().row_prompt(0),
        marspot::grid::PromptMark::PromptStart
    );
    t.feed(b"\r\n\r\n\r\n\r\n");
    assert_eq!(
        t.grid().scrollback_prompt(0),
        marspot::grid::PromptMark::PromptStart
    );
    // nothing outside the engine reads a mark: no key, menu, panel or
    // sidebar
    for (file, text) in app_sources() {
        for call in ["row_prompt(", "scrollback_prompt(", "PromptMark::"] {
            assert!(
                !text.contains(call),
                "{}",
                closed(&format!("{file} reads prompt marks ({call})"))
            );
        }
    }
    // and no command uses it: the table is every command a key or a
    // menu can run
    let commands = marspot::commands::TABLE;
    assert!(commands.len() > 20, "read no commands out of the table");
    for row in commands {
        let name = format!("{:?}", row.command);
        for word in [
            "Prompt",
            "Jump",
            "Mark",
            "PrevCommand",
            "PreviousCommand",
            "NextCommand",
        ] {
            assert!(
                !name.contains(word),
                "{}",
                closed(&format!("the command table has {name}"))
            );
        }
    }

    // OSC 52: written, never read back
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(t.take_osc_clipboard().as_deref(), Some("hello"));
    t.feed(b"\x1b]52;c;?\x07");
    assert!(
        t.take_response().is_empty(),
        "{}",
        closed("OSC 52 answered a read")
    );
    assert!(
        t.take_osc_clipboard().is_none(),
        "a read query landed on the clipboard"
    );
}

/// The settings model is what the panel draws; it has the theme and
/// the language and nothing for fonts or keys.
#[test]
fn fonts_font_size_and_keys_are_not_settings() {
    says("The settings panel offers the theme (dark, light, high contrast), the language");
    says("Fonts, font size and key bindings are not configurable yet");
    the_page_says("Fonts, font size and key bindings are not configurable yet");
    let keys = marspot::settings::render(&marspot::settings::Settings::default(), "");
    for k in ["appearance.theme", "appearance.language"] {
        assert!(keys.contains(k), "settings.toml has no {k}");
    }
    let names: Vec<&str> = keys
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split(" = ").next())
        .collect();
    assert!(
        names.len() >= 5,
        "read no keys out of settings.toml: {names:?}"
    );
    for n in &names {
        let n = n.to_ascii_lowercase();
        for word in ["font", "key", "bind", "shortcut", "size"] {
            assert!(
                !n.contains(word),
                "{}",
                closed(&format!("settings.toml has {n}"))
            );
        }
    }
    let themes: Vec<&str> = marspot::palette::ThemeId::ALL
        .iter()
        .map(|t| t.as_str())
        .collect();
    assert_eq!(themes.len(), 3, "the README names three themes: {themes:?}");
}

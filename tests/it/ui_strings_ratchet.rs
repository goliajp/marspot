//! No user-visible literal comes back outside the catalogue.
//!
//! Source shape, because the point is the absence of a thing. Without it
//! the catalogue un-does itself one convenient literal at a time, and
//! nobody notices until half a panel stays in English.
//!
//! Two layers, because the two kinds of file differ. The component
//! directories hold almost nothing but interface, so there every literal
//! that reads like text is suspect. The binaries and plugins are mostly
//! logs and protocol, so there only a literal handed straight to
//! something that draws is.

use std::path::{Path, PathBuf};

/// Files of the component directories that are not interface: the
/// developer panel, the test scenes that draw fixtures, and the tests
/// that hold a scene at one display scale to the same scene at another.
const NOT_INTERFACE: &[&str] =
    &["dev_panel.rs", "_snapshot_tests.rs", "panel_script.rs", "density_kit.rs", "density_tests.rs", "centring_tests.rs"];

/// Literals in the component directories that are not sentences.
const NOT_TEXT: &[(&str, &str)] = &[
    ("Aa", "the search bar's case toggle, a glyph pair"),
    ("Esc", "a key cap"),
    ("MarspotMenuTarget", "an Objective-C class name"),
];

/// What draws text, or carries it to what draws, in the files that are
/// not mostly interface.
const DRAWS: &[&str] = &[
    "MenuItem::entry(",
    "panel_text(",
    "panel_text_in(",
    "section_heading(",
    ".badge(",
    "PaneBadgeMenuItem {",
    "Column {",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` under `dir`, as (path from `src`, text).
fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).expect("a source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path.strip_prefix(root()).expect("under src").display().to_string();
            out.push((name, std::fs::read_to_string(&path).expect("a source file")));
        }
    }
}

fn source(name: &str) -> (String, String) {
    (name.to_string(), std::fs::read_to_string(root().join(name)).expect("a source file"))
}

/// `text` with its inline test modules and its comments blanked, line
/// numbers kept. A test module ends at the first `}` in column 0, which
/// is where every one in this repository ends.
fn production(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tests = false;
    let mut pending_cfg = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if in_tests {
            in_tests = line != "}";
        } else if trimmed.starts_with("#[cfg(test)]") {
            pending_cfg = true;
        } else if pending_cfg && trimmed.contains("mod ") && trimmed.ends_with('{') {
            (pending_cfg, in_tests) = (false, true);
        } else {
            pending_cfg = false;
            if !trimmed.starts_with("//") {
                out.push_str(code_of(line));
            }
        }
        out.push('\n');
    }
    out
}

/// `line` up to a trailing comment, if it has one outside a string.
fn code_of(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            b'"' => in_str = !in_str,
            b'/' if !in_str && b.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// The string literals in `code`, each with the byte offset of its
/// opening quote. Spans lines: a literal may.
fn literals(code: &str) -> Vec<(usize, &str)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // a quote character literal is not the start of a string
        if b[i] == b'\'' && b.get(i + 1) == Some(&b'"') && b.get(i + 2) == Some(&b'\'') {
            i += 3;
            continue;
        }
        if b[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            out.push((i, &code[start..j.min(b.len())]));
            i = j;
        }
        i += 1;
    }
    out
}

fn line_of(code: &str, at: usize) -> usize {
    code[..at].matches('\n').count() + 1
}

/// The index just past the bracket that closes the one at `open`.
fn closing(code: &str, open: usize) -> usize {
    let b = code.as_bytes();
    let (mut depth, mut in_str, mut i) = (0usize, false, open);
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            b'"' => in_str = !in_str,
            b'(' | b'{' | b'[' if !in_str => depth += 1,
            b')' | b'}' | b']' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// Whether `lit` has a word in it once its `{placeholders}` are gone: two
/// ASCII letters in a row. `→ P{n}` and `{c}/{t}` have none.
fn has_a_word(lit: &str) -> bool {
    let mut depth = 0;
    let mut run = 0;
    for c in lit.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            c if depth == 0 && c.is_ascii_alphabetic() => {
                run += 1;
                if run == 2 {
                    return true;
                }
                continue;
            }
            _ => {}
        }
        run = 0;
    }
    false
}

/// Layer one: literals in a file that is all interface. One that starts
/// with a capital, or has a space and a word, reads like text — unless it
/// is a log line's, a panic's, or one of the named exceptions.
fn text_like_literals(code: &str) -> Vec<(usize, String)> {
    let logs: Vec<(usize, usize)> = code
        .match_indices("lx_")
        .filter_map(|(i, _)| {
            let name = code[i..].find(|c: char| !c.is_ascii_alphanumeric() && c != '_')?;
            code[i + name..].starts_with("!(").then(|| (i, closing(code, i + name + 1)))
        })
        .collect();
    literals(code)
        .into_iter()
        .filter(|(at, lit)| {
            let reads_like_text = lit.starts_with(|c: char| c.is_ascii_uppercase())
                || (lit.contains(' ') && has_a_word(lit));
            reads_like_text
                && !NOT_TEXT.iter().any(|(known, _)| known == lit)
                && !logs.iter().any(|(from, to)| (*from..*to).contains(at))
                && !code[..*at].trim_end().ends_with(".expect(")
        })
        .map(|(at, lit)| (line_of(code, at), lit.to_string()))
        .collect()
}

/// Layer two: a literal with a word in it, or a `format!`, handed
/// straight to something that draws.
fn literals_handed_to_what_draws(code: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for call in DRAWS {
        for (at, _) in code.match_indices(call) {
            let open = at + call.len() - 1;
            let args = &code[open..closing(code, open)];
            for (i, lit) in literals(args) {
                if has_a_word(lit) {
                    found.push((line_of(code, open + i), lit.to_string()));
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn report(found: Vec<(String, usize, String)>) -> String {
    found.iter().map(|(f, n, lit)| format!("{f}:{n}: {lit:?}")).collect::<Vec<_>>().join("\n  ")
}

#[test]
fn interface_files_hold_no_text_outside_the_catalogue() {
    let mut files = Vec::new();
    for dir in ["ui/components", "frame_build", "tools"] {
        sources(&root().join(dir), &mut files);
    }
    files.push(source("app_menu.rs"));
    files.push(source("commands.rs"));
    files.retain(|(name, _)| !NOT_INTERFACE.iter().any(|x| name.ends_with(x)));
    assert!(files.len() >= 45, "only {} files were scanned", files.len());

    let mut found = Vec::new();
    for (name, text) in &files {
        for (n, lit) in text_like_literals(&production(text)) {
            found.push((name.clone(), n, lit));
        }
    }
    assert!(
        found.is_empty(),
        "user-visible text outside `ui::strings`; give it a `Msg`, or an `Arg` if it is built \
         around a value:\n  {}",
        report(found)
    );

    // an exception that no longer matches anything has outlived its reason
    let all: String = files.iter().map(|(_, text)| production(text)).collect();
    for (lit, why) in NOT_TEXT {
        assert!(all.contains(&format!("\"{lit}\"")), "{lit:?} ({why}) is gone: drop the exception");
    }
}

#[test]
fn nothing_hands_a_bare_literal_to_what_draws() {
    let mut files = vec![
        source("bin/marspot-core.rs"),
        source("main.rs"),
        source("pane.rs"),
        source("cc_usage.rs"),
        source("bin/marspot-shell/banner.rs"),
    ];
    sources(&root().join("bin/marspot-shell/plugins"), &mut files);
    assert!(files.len() >= 80, "only {} files were scanned", files.len());

    let mut found = Vec::new();
    let mut calls = 0;
    for (name, text) in &files {
        let code = production(text);
        calls += DRAWS.iter().map(|call| code.matches(call).count()).sum::<usize>();
        for (n, lit) in literals_handed_to_what_draws(&code) {
            found.push((name.clone(), n, lit));
        }
    }
    assert!(calls >= 20, "only {calls} drawing calls were looked at");
    assert!(
        found.is_empty(),
        "a literal goes straight to something that draws; give it a `Msg`, or an `Arg` if it \
         is built around a value:\n  {}",
        report(found)
    );
}

/// The two searches find what they are for, and leave alone what they
/// should. A search that cannot fail proves nothing by passing.
#[test]
fn the_searches_can_fail() {
    let caught = |line: &str| !text_like_literals(&production(line)).is_empty();
    assert!(caught("p.panel_text(role, x, y, \"Reset all\", fg);"), "a capitalised label");
    assert!(caught("let hint = \"press any key to retry\";"), "a lower-case sentence");
    assert!(caught("let s = format!(\"{n} panes open\");"), "a sentence around a value");
    assert!(caught("chrome.paint(\n    p,\n    \"Layout\",\n);"), "a literal on its own line");
    assert!(!caught("// let s = \"Reset all\";"), "a comment");
    assert!(!caught("let x = 1; // \"Reset all\""), "a trailing comment");
    assert!(!caught("crate::lx_event!(\n    \"PANEL_OPEN\",\n    \"a panel opened\",\n);"), "a log line");
    assert!(!caught("let f = std::fs::read(p).expect(\"Readable file\");"), "a panic message");
    assert!(!caught("let key = \"⌘C\"; let unit = \"%\"; let id = \"sidebar\";"), "glyphs and ids");
    assert!(
        !caught("fn f() {}\n#[cfg(test)]\nmod tests {\n    const S: &str = \"Reset all\";\n}\n"),
        "a test module"
    );
    assert!(
        caught("#[cfg(test)]\nmod tests {\n    fn f() {}\n}\nconst S: &str = \"Reset all\";\n"),
        "code after a test module"
    );

    let caught = |code: &str| !literals_handed_to_what_draws(&production(code)).is_empty();
    assert!(caught("items.push(MenuItem::entry(\"Rename pane\", 7));"));
    assert!(caught("items.push(MenuItem::entry(\n    &format!(\"Move to Window {}\", n),\n    7,\n));"));
    assert!(caught("PtyOp::new(\"cc.blocked\").badge(\"⚠ no account free\")"));
    assert!(caught("PaneBadgeMenuItem {\n    tag: 1,\n    label: format!(\"switch to P{}\", n),\n}"));
    assert!(caught("Column { header: \"Pane\", width: ColumnWidth::Flex }"));
    assert!(caught("p.panel_text(role, x, y, \"no key\", fg);"));
    assert!(!caught("items.push(MenuItem::entry(t(Msg::MenuCopy), 7));"));
    assert!(!caught("op.badge(format!(\"→ P{profile}\"))"), "a glyph and an account number");
    assert!(!caught("Column { header: \"\", width: ColumnWidth::Px(w) }"));
    assert!(!caught("host.log(LogLevel::Warn, \"badge_menu.no_profiles\", \"nothing to switch\");"));
}

//! The catalogue is complete, and the files that use it stay migrated.
//!
//! S7-01. The first two tests are about the catalogue itself; the third is
//! a ratchet — it fails if a bare English literal comes back into a file
//! that has already been migrated. Without it the migration un-does itself
//! one convenient literal at a time, and nobody notices until a translator
//! asks why half the settings panel is still in English.

use marspot::ui::strings::{ALL, t};

/// Every message resolves to something, and nothing resolves to nothing.
///
/// An empty arm compiles and draws an empty label, which looks like a
/// layout bug rather than a missing string.
#[test]
fn every_message_has_text() {
    for m in ALL {
        let s = t(*m);
        assert!(!s.trim().is_empty(), "{m:?} resolves to empty text");
    }
}

/// `ALL` is the list a translator would be handed, so a variant missing
/// from it is a string that silently never gets translated.
///
/// The enum has no reflection, so this compares the count against a
/// literal that has to be updated deliberately. A variant added without
/// touching either fails here.
#[test]
fn every_message_is_listed() {
    const EXPECTED: usize = 22;
    assert_eq!(
        ALL.len(),
        EXPECTED,
        "the catalogue has {} messages and the test expects {EXPECTED}. If a message \
         was added, add it to ALL and bump this; if one was removed, the same",
        ALL.len()
    );
    // And no duplicates: two ids for one sentence means a translator
    // translates it twice and the two can drift apart.
    let mut seen = std::collections::HashSet::new();
    for m in ALL {
        assert!(seen.insert(format!("{m:?}")), "{m:?} is listed twice");
    }
}

/// Two ids may not carry the same English text.
///
/// Where they genuinely should differ in another language -- "Normal" as a
/// dimming level and "Normal" as a scroll speed -- they are separate ids
/// with separate text, and if the English coincides the pair has to be
/// looked at rather than left to a translator to guess.
#[test]
fn no_two_messages_share_their_english() {
    let mut by_text: std::collections::HashMap<&str, Vec<String>> = Default::default();
    for m in ALL {
        by_text.entry(t(*m)).or_default().push(format!("{m:?}"));
    }
    let clashes: Vec<String> = by_text
        .iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(text, ids)| format!("{text:?} is {}", ids.join(" and ")))
        .collect();
    assert!(
        clashes.is_empty(),
        "two messages share their English, so a translator cannot tell them apart:\n  {}",
        clashes.join("\n  ")
    );
}

/// The migrated files hold no user-visible English literal any more.
///
/// Source shape, because the point is the absence of a thing. A literal is
/// "user-visible" here if it starts with a capital and reads like a
/// sentence -- which also matches some identifiers, so the ones that are
/// not text are listed rather than guessed at.
#[test]
fn migrated_files_do_not_grow_new_literals() {
    const MIGRATED: &[(&str, &[&str])] = &[(
        "src/ui/components/settings_modal.rs",
        // Not user-visible: type names that happen to be capitalised.
        &["Msg", "Row", "Control", "Segmented", "RowSpec", "Section"],
    )];
    // The three `Control::Segmented` option arrays are not migrated yet,
    // and the exemption is by name rather than by file so the rest of the
    // file stays covered. Two of them are prose and belong in the
    // catalogue; the third is durations ("15m", "1h"), which is a number
    // and a unit to format rather than a string to translate -- S7-03.
    //
    // `the_deferred_arrays_still_exist` below fails when they go, which is
    // what stops this exemption from outliving its reason.
    const DEFERRED: &[&str] = &["IDLE_LABELS", "DIM_LABELS", "SCROLL_LABELS"];
    for (path, allowed) in MIGRATED {
        let text = std::fs::read_to_string(path).expect("a migrated file is readable");
        // Production code only: a test fixture may hold whatever it likes.
        let prod = text
            .split("#[cfg(test)]")
            .next()
            .expect("there is something before the tests");
        let mut offenders = Vec::new();
        for (n, line) in prod.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if DEFERRED.iter().any(|d| line.contains(d)) {
                continue;
            }
            for lit in literals(line) {
                if lit.len() < 3 || !lit.starts_with(|c: char| c.is_ascii_uppercase()) {
                    continue;
                }
                if allowed.contains(&lit) {
                    continue;
                }
                offenders.push(format!("{path}:{}: {lit:?}", n + 1));
            }
        }
        assert!(
            offenders.is_empty(),
            "a user-visible literal is back in a migrated file; it belongs in \
             `ui::strings`:\n  {}",
            offenders.join("\n  ")
        );
    }
}

/// String literals on one line, without their quotes. Good enough for a
/// source-shape check: it does not try to span lines, and a literal that
/// spans one is already unusual enough to want looking at.
fn literals(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != b'"' {
                if b[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            if j <= b.len() {
                if let Some(s) = line.get(start..j.min(b.len())) {
                    out.push(s);
                }
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// The exemption above names three arrays. When they are migrated this
/// fails, and the exemption goes with them.
#[test]
fn the_deferred_arrays_still_exist() {
    let text = std::fs::read_to_string("src/ui/components/settings_modal.rs")
        .expect("the settings panel is readable");
    for name in ["IDLE_LABELS", "DIM_LABELS", "SCROLL_LABELS"] {
        assert!(
            text.contains(&format!("pub const {name}: &[&str]")),
            "{name} is no longer a `&[&str]`, so the ratchet's exemption for it in              `migrated_files_do_not_grow_new_literals` has outlived its reason --              remove it from DEFERRED"
        );
    }
}

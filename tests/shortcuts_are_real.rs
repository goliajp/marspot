//! Every documented shortcut has a handler, and the handler is where
//! the list says it is.
//!
//! A keyboard reference is the kind of documentation that goes quietly
//! wrong: the chord is removed or moved, the page keeps promising it,
//! and nobody finds out until a user presses it.  This binds the list
//! to the code — not by parsing the handler, which would be its own
//! fiction, but by requiring the literal that handler matches on to
//! still be in the file the list names.

use marspot::shortcuts::SHORTCUTS;

#[test]
fn every_shortcut_points_at_a_handler_that_exists() {
    assert!(!SHORTCUTS.is_empty(), "an empty list would pass every check below");

    let mut checked = 0;
    for s in SHORTCUTS {
        let src = std::fs::read_to_string(s.handler_file)
            .unwrap_or_else(|e| panic!("{} names {}: {e}", s.chord, s.handler_file));
        assert!(
            src.contains(s.handler_match),
            "{} ({}) says its handler matches on `{}` in {}, and it does not",
            s.chord, s.action, s.handler_match, s.handler_file,
        );
        checked += 1;
    }
    assert_eq!(checked, SHORTCUTS.len());
}

#[test]
fn the_check_would_notice_a_handler_that_went_away() {
    // The assertion above passes for every entry, which reads the same
    // whether it is checking anything or the file just happens to
    // contain every string handed to it.  This one hands it a literal
    // no source file has.
    let src = std::fs::read_to_string("src/bin/marspot-core.rs").expect("read");
    assert!(
        !src.contains("eq_ignore_ascii_case(&'\u{1}')"),
        "the file contains a literal chosen for not being in it"
    );
}

#[test]
fn no_two_shortcuts_claim_the_same_chord() {
    let mut seen: Vec<&str> = Vec::new();
    for s in SHORTCUTS {
        assert!(
            !seen.contains(&s.chord),
            "{} is listed twice; one of them is not what happens",
            s.chord
        );
        seen.push(s.chord);
    }
}

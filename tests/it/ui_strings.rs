//! The catalogue is complete and says what it said.
//!
//! The tests here are about the catalogue itself. What keeps a bare
//! literal from coming back into the code that draws is the ratchet in
//! `ui_strings_ratchet`.

use marspot::ui::strings::{self, t, Arg, MonthDayTime, Msg, ALL};

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
    const EXPECTED: usize = 122;
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

/// Two ids may not carry the same English text, unless the pair is named
/// here as two meanings that only English spells alike.
///
/// "Normal" as a dimming level and "Normal" as a wheel speed are two
/// words in a language that has two; so are "Light" as a dimming level
/// and "Light" as a theme. Any other coincidence has to be looked at
/// rather than left to a translator to guess.
#[test]
fn no_two_messages_share_their_english() {
    const TWO_MEANINGS: &[(&str, &[Msg])] = &[
        ("Normal", &[Msg::DimNormal, Msg::ScrollNormal]),
        ("Light", &[Msg::DimLight, Msg::ThemeLight]),
    ];
    let mut by_text: std::collections::HashMap<&str, Vec<Msg>> = Default::default();
    for m in ALL {
        by_text.entry(t(*m)).or_default().push(*m);
    }
    let clashes: Vec<String> = by_text
        .iter()
        .filter(|(text, ids)| {
            ids.len() > 1 && !TWO_MEANINGS.iter().any(|(en, pair)| en == *text && pair == &ids.as_slice())
        })
        .map(|(text, ids)| format!("{text:?} is {ids:?}"))
        .collect();
    assert!(
        clashes.is_empty(),
        "two messages share their English, so a translator cannot tell them apart:\n  {}",
        clashes.join("\n  ")
    );
    for (en, pair) in TWO_MEANINGS {
        assert_eq!(by_text.get(en).map(Vec::as_slice), Some(*pair), "{en:?} is no longer shared: drop it from the list");
    }
}

/// One of each sentence built around a value, with what English makes of
/// it. A language added later is run over the same values.
const SAMPLES: &[(Arg<'static>, &str)] = &[
    (Arg::MoveToWindow(2), "Move to Window 2"),
    (Arg::PaneCount { cols: 4, rows: 1 }, "4 × 1 = 4 panes"),
    (Arg::PaneCount { cols: 1, rows: 1 }, "1 × 1 = 1 pane"),
    (Arg::ProcessDetail { pane: "api", sid: 7, shell_pid: 4242 }, "api · sid 7 · shell pid 4242"),
    (Arg::ResetAt(MonthDayTime { month: 1, day: 2, hour: 6, minute: 0 }), "reset: 1/2 06:00"),
    (Arg::UpdatedAt(MonthDayTime { month: 11, day: 10, hour: 18, minute: 5 }), "updated 11/10 18:05"),
    (Arg::SessionStarting(12), "session 12 starting…"),
    (Arg::SessionUnavailable(12), "session 12 unavailable — press any key to retry"),
    (Arg::ProfileRow { n: 2, current: true }, "● profile 2"),
    (Arg::ProfileRow { n: 3, current: false }, "   profile 3"),
    (Arg::HandOffTo("codex P2"), "hand off to codex P2"),
    (Arg::SwitchToProfile(4), "switch to P4"),
];

/// The sentences built around a value read as they did when each call
/// site glued its own together.
#[test]
fn sentences_with_values_read_as_before() {
    for (arg, want) in SAMPLES {
        assert_eq!(strings::text(*arg), *want, "{arg:?}");
    }
}

/// A caller that keeps its buffer gets the sentence appended to it and
/// nothing else done to it.
#[test]
fn a_sentence_is_written_into_the_callers_buffer() {
    let mut out = String::with_capacity(64);
    let before = out.capacity();
    strings::write(&mut out, Arg::MoveToWindow(3)).expect("a String takes any text");
    out.push('|');
    strings::write(&mut out, Arg::SwitchToProfile(1)).expect("a String takes any text");
    assert_eq!(out, "Move to Window 3|switch to P1");
    assert_eq!(out.capacity(), before, "the buffer was big enough and was not replaced");
}

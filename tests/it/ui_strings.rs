//! The catalogue is complete and says what it said.
//!
//! The tests here are about the catalogue itself. What keeps a bare
//! literal from coming back into the code that draws is the ratchet in
//! `ui_strings_ratchet`.

use marspot::ui::strings::{self, t, Arg, MonthDayTime, Msg, ALL};
use marspot_term::language::LangId;

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
    const EXPECTED: usize = 128;
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

/// One of each sentence built around a value: what English makes of
/// it, and the values that have to appear in it whatever the language.
const SAMPLES: &[(Arg<'static>, &str, &[&str])] = &[
    (Arg::MoveToWindow(2), "Move to Window 2", &["2"]),
    (Arg::PaneCount { cols: 4, rows: 1 }, "4 × 1 = 4 panes", &["4 × 1 = 4"]),
    (Arg::PaneCount { cols: 1, rows: 1 }, "1 × 1 = 1 pane", &["1 × 1 = 1"]),
    (
        Arg::ProcessDetail { pane: "api", sid: 7, shell_pid: 4242 },
        "api · sid 7 · shell pid 4242",
        &["api", "7", "4242"],
    ),
    (
        Arg::ResetAt(MonthDayTime { month: 1, day: 2, hour: 6, minute: 0 }),
        "reset: 1/2 06:00",
        &["1/2 06:00"],
    ),
    (
        Arg::UpdatedAt(MonthDayTime { month: 11, day: 10, hour: 18, minute: 5 }),
        "updated 11/10 18:05",
        &["11/10 18:05"],
    ),
    (Arg::SessionStarting(12), "session 12 starting…", &["12"]),
    (Arg::SessionUnavailable(12), "session 12 unavailable — press any key to retry", &["12"]),
    (Arg::ProfileRow { n: 2, current: true }, "● profile 2", &["●", "2"]),
    (Arg::ProfileRow { n: 3, current: false }, "   profile 3", &["3"]),
    (Arg::HandOffTo("codex P2"), "hand off to codex P2", &["codex P2"]),
    (Arg::SwitchToProfile(4), "switch to P4", &["P4"]),
];

/// The sentences built around a value read as they did when each call
/// site glued its own together.
#[test]
fn sentences_with_values_read_as_before() {
    for (arg, want, _) in SAMPLES {
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

// ─── the translations ────────────────────────────────────────────

const TRANSLATED: [LangId; 2] = [LangId::Ja, LangId::ZhHans];

fn text_in(lang: LangId, m: Msg) -> &'static str {
    (strings::table(lang).text)(m)
}

fn written_in(lang: LangId, a: Arg<'_>) -> String {
    let mut out = String::new();
    (strings::table(lang).write)(a, &mut out).expect("a String takes any text");
    out
}

/// The messages that are sentences: the line under a settings row, and
/// what an empty panel says. Everything else is a label — a title, a
/// menu item, a button, a segment, a table header, a status word, a
/// banner — and a label takes no full stop.
fn is_sentence(m: Msg) -> bool {
    matches!(
        m,
        Msg::CostReclaimIdlePanes
            | Msg::CostIdleFor
            | Msg::CostWarmUpOnReturn
            | Msg::CostTheme
            | Msg::CostDimOtherPanes
            | Msg::CostCircledDigitsWide
            | Msg::CostClaudeCodeReportsModel
            | Msg::CostWheelSpeed
            | Msg::CostLanguage
            | Msg::CostLanguageRelaunch
            | Msg::AccountsEmpty
    )
}

/// Text that reads the same in every language: names, and units nobody
/// translates.
const SAME_EVERYWHERE: &[&str] = &["Marspot", "Claude Code", "CPU%", "RSS", "pid", "English"];

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF     // kana
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF // Han
        | 0x3000..=0x303F   // CJK punctuation
        | 0xFF00..=0xFFEF)  // full-width forms
}

/// What is wrong with `s` as Japanese or Chinese interface text, if
/// anything.
fn typography_fault(s: &str, label: bool) -> Option<String> {
    if label && s.ends_with(['。', '.', '！', '!']) {
        return Some("a label ends with a full stop".into());
    }
    if !label && !s.ends_with('。') {
        return Some("a sentence does not end with 。".into());
    }
    if s.contains("...") {
        return Some("three dots instead of …".into());
    }
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let beside_cjk = (i > 0 && is_cjk(chars[i - 1])) || chars.get(i + 1).is_some_and(|n| is_cjk(*n));
        if beside_cjk && ",;:!?()".contains(*c) {
            return Some(format!("half-width {c:?} beside a CJK character"));
        }
    }
    None
}

#[test]
fn every_message_has_text_in_every_language() {
    for lang in LangId::ALL {
        for m in ALL {
            assert!(!text_in(lang, *m).trim().is_empty(), "{m:?} is empty in {lang:?}");
        }
        for (arg, _, _) in SAMPLES {
            assert!(!written_in(lang, *arg).trim().is_empty(), "{arg:?} is empty in {lang:?}");
        }
    }
}

/// A value handed to a sentence appears in it, in every language: a
/// translation that drops the window number still reads fluently.
#[test]
fn every_value_appears_in_its_sentence_in_every_language() {
    for lang in LangId::ALL {
        for (arg, _, values) in SAMPLES {
            let out = written_in(lang, *arg);
            for v in *values {
                assert!(out.contains(v), "{lang:?}: {arg:?} reads {out:?}, without {v:?}");
            }
        }
    }
}

#[test]
fn translations_are_translated() {
    for lang in TRANSLATED {
        for m in ALL {
            let s = text_in(lang, *m);
            let same = SAME_EVERYWHERE.contains(&s) || matches!(m, Msg::LangJapanese | Msg::LangChinese);
            assert!(same || !s.is_ascii(), "{lang:?}: {m:?} is still {s:?}");
        }
        for (arg, _, _) in SAMPLES {
            let out = written_in(lang, *arg);
            assert!(!out.is_ascii(), "{lang:?}: {arg:?} is still {out:?}");
        }
    }
}

/// Labels take no full stop, sentences end with 。, punctuation beside
/// CJK text is full-width, and an ellipsis is one character.
#[test]
fn translations_follow_cjk_typography() {
    let mut faults = Vec::new();
    for lang in TRANSLATED {
        for m in ALL {
            let s = text_in(lang, *m);
            if let Some(why) = typography_fault(s, !is_sentence(*m)) {
                faults.push(format!("{lang:?} {m:?} {s:?}: {why}"));
            }
        }
        for (arg, _, _) in SAMPLES {
            let out = written_in(lang, *arg);
            if let Some(why) = typography_fault(&out, true) {
                faults.push(format!("{lang:?} {arg:?} {out:?}: {why}"));
            }
        }
    }
    assert!(faults.is_empty(), "\n  {}", faults.join("\n  "));

    // the check finds what it is for
    assert!(typography_fault("设置。", true).is_some(), "a label with a full stop");
    assert!(typography_fault("設定.", true).is_some());
    assert!(typography_fault("设置", true).is_none());
    assert!(typography_fault("立即应用", false).is_some(), "a sentence without one");
    assert!(typography_fault("立即应用。", false).is_none());
    assert!(typography_fault("已进入安全模式(保留会话)", true).is_some(), "half-width brackets");
    assert!(typography_fault("返回时,唤醒", true).is_some(), "a half-width comma");
    assert!(typography_fault("正在恢复...", true).is_some(), "three dots");
    assert!(typography_fault("Marspot 正在恢复…", true).is_none());
}

/// The whole catalogue as `key<TAB>en<TAB>ja<TAB>zh-Hans`, for whoever
/// reviews the translations: written to the file `MARSPOT_CATALOGUE_TSV`
/// names, when it names one.
#[test]
fn the_catalogue_can_be_listed_for_review() {
    let mut out = String::from("key\ten\tja\tzh-Hans\n");
    for m in ALL {
        out += &format!("{m:?}");
        for lang in LangId::ALL {
            out += &format!("\t{}", text_in(lang, *m));
        }
        out.push('\n');
    }
    for (arg, _, _) in SAMPLES {
        out += &format!("{arg:?}");
        for lang in LangId::ALL {
            out += &format!("\t{}", written_in(lang, *arg));
        }
        out.push('\n');
    }
    assert_eq!(out.lines().count(), 1 + ALL.len() + SAMPLES.len());
    assert!(out.lines().all(|l| l.split('\t').count() == 4), "a tab or a line break inside a message");
    if let Some(path) = std::env::var_os("MARSPOT_CATALOGUE_TSV") {
        std::fs::write(path, out).expect("the listing is writable");
    }
}

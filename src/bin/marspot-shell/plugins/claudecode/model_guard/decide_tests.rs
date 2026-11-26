use super::*;

const OPUS: &str = "{\n  \"model\": \"opus\",\n  \"theme\": \"dark\"\n}\n";
const FABLE: &str = "{\n  \"model\": \"fable\",\n  \"theme\": \"dark\"\n}\n";
const BARE: &str = "{\n  \"theme\": \"dark\"\n}\n";
const LIGHT: &str = "{\n  \"theme\": \"light\"\n}\n";

fn w(t: &str) -> Seen {
    Seen::of_text(t.to_string())
}

fn m(s: &str) -> Option<Value> {
    Some(Value::Str(s.to_string()))
}

#[test]
fn a_reading_is_whole_only_as_a_json_object() {
    assert_eq!(w(OPUS), Seen::Whole { text: OPUS.into(), model: m("opus") });
    assert_eq!(w(BARE), Seen::Whole { text: BARE.into(), model: None });
    for t in ["", "  \n", "{\"model\": \"op", "[1]"] {
        assert_eq!(w(t), Seen::Partial(t.into()), "{t:?}");
    }
}

#[test]
fn an_empty_missing_or_half_written_file_after_a_whole_one_is_waited_out() {
    for now in [w(""), w("{\"model\": "), Seen::Absent, Seen::Unreadable] {
        assert_eq!(act(&w(OPUS), &now, true, false), Act::Wait, "{now:?}");
        assert_eq!(act(&w(BARE), &now, true, false), Act::Wait, "{now:?}");
    }
    // a file known to have no model keeps that through a write half way
    assert_eq!(act(&Seen::Absent, &w("{\"mo"), true, false), Act::Wait);
    assert_eq!(act(&w(""), &Seen::Unreadable, true, false), Act::Wait);
    assert_eq!(act(&Seen::Absent, &w(""), true, false), Act::Adopt);
}

#[test]
fn nothing_is_done_from_an_unreadable_or_unknown_reading() {
    assert_eq!(act(&Seen::Unreadable, &w(FABLE), true, false), Act::Adopt);
    assert_eq!(act(&w("{\"x"), &w(FABLE), true, false), Act::Adopt);
    assert_eq!(act(&Seen::Unreadable, &w("{\"x"), true, false), Act::Adopt);
}

#[test]
fn under_managed_a_changed_model_is_for_the_guard_and_under_manual_kept() {
    assert_eq!(act(&w(BARE), &w(FABLE), true, false), Act::PutBack { to: m("fable"), back: None });
    assert_eq!(act(&w(OPUS), &w(FABLE), true, false), Act::PutBack { to: m("fable"), back: m("opus") });
    assert_eq!(act(&w(OPUS), &w(BARE), true, false), Act::PutBack { to: None, back: m("opus") });
    assert_eq!(act(&Seen::Absent, &w(FABLE), true, false), Act::PutBack { to: m("fable"), back: None });
    assert_eq!(act(&w(""), &w(FABLE), true, false), Act::PutBack { to: m("fable"), back: None });
    assert_eq!(act(&w(OPUS), &w(FABLE), false, false), Act::Keep { from: m("opus"), to: m("fable") });
}

#[test]
fn an_own_write_is_quiet_unless_it_changes_the_model() {
    assert_eq!(act(&w(BARE), &w(LIGHT), true, true), Act::Adopt);
    assert_eq!(act(&w(OPUS), &w(FABLE), true, true), Act::PutBack { to: m("fable"), back: m("opus") });
}

#[test]
fn other_changes_are_only_said() {
    assert_eq!(act(&w(BARE), &w(LIGHT), true, false), Act::Tell);
    assert_eq!(act(&w(BARE), &w(LIGHT), false, false), Act::Tell);
    assert_eq!(act(&w(OPUS), &w(OPUS), true, false), Act::Nothing);
}

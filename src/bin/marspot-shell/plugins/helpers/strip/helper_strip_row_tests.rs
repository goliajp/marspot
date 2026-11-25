//! What a strip row calls a helper and says it does.

use marspot::shell_proto::StripAgent;
use marspot_peer::control::AgentKind;

use super::super::table::Event;
use super::super::tests::{feed, shared_at, started, temp};
use super::helper_strip_tests::{begin, dir_gone, panes, strips};
use super::*;

#[test]
fn a_row_names_agent_and_model_and_has_a_plain_task() {
    let d = temp("strip-row");
    let (s, _rx) = shared_at(&d);
    let mut st = started(10, "claude", AgentKind::Claude);
    (st.model, st.task_line) = ("opus".into(), "看\tthe \x1b[31mparser".into());
    feed(&s, 1, vec![Event::started(st), Event::TaskDelivered]);
    let got = strips(&s, &mut Memory::default(), &panes(&[(10, 0), (1, 10)]));
    let r = &got[0].rows[0];
    assert_eq!((r.name.as_str(), r.model.as_str(), r.task.as_str()), ("claude opus", "opus", "看the [31mparser"));
    assert_eq!((r.agent, r.ended_ms, r.tokens), (StripAgent::Claude, None, None));
    begin(&s, 2, 10, AgentKind::Codex);
    let got = strips(&s, &mut Memory::default(), &panes(&[(10, 0), (1, 10), (2, 10)]));
    assert_eq!((got[0].rows[1].name.as_str(), got[0].rows[1].model.as_str()), ("codex", ""));
    // a name it was given is no name for the row, but says what it does
    // when no title does
    let mut st = started(10, "claude", AgentKind::Claude);
    (st.model, st.label) = ("claude-haiku".into(), "par\u{7}ser".into());
    feed(&s, 3, vec![Event::started(st), Event::TaskDelivered]);
    let mut st = started(10, "codex", AgentKind::Codex);
    (st.model, st.label, st.title) = ("gpt-6".into(), "rev".into(), "review the \u{1b}lexer".into());
    feed(&s, 4, vec![Event::started(st), Event::TaskDelivered]);
    let got = strips(&s, &mut Memory::default(), &panes(&[(10, 0), (1, 10), (2, 10), (3, 10), (4, 10)]));
    let rows: Vec<(&str, &str)> = got[0].rows.iter().map(|r| (r.name.as_str(), r.task.as_str())).collect();
    assert_eq!(
        rows,
        [("claude opus", "看the [31mparser"), ("codex", "reviewthe \\ parser"), ("claude haiku", "parser"), ("codex gpt-6", "review the lexer")]
    );
    dir_gone(&d);
}

#[test]
fn a_row_names_the_model_its_records_say_answered_and_cuts_a_given_name() {
    let d = temp("strip-model");
    let (s, _rx) = shared_at(&d);
    let mut st = started(10, "claude", AgentKind::Claude);
    (st.model, st.label) = ("default".into(), format!("first line\nsecond line {}", "x".repeat(200)));
    feed(&s, 1, vec![Event::started(st), Event::TaskDelivered]);
    let mut st = started(10, "claude", AgentKind::Claude);
    st.label = "y".repeat(300);
    feed(&s, 2, vec![Event::started(st), Event::TaskDelivered]);
    let obs = panes(&[(10, 0), (1, 10), (2, 10)]);
    let name = |s: &Shared| strips(s, &mut Memory::default(), &obs)[0].rows[0].name.clone();
    assert_eq!(name(&s), "claude", "default: the agent alone, until a record names one");
    s.used(1, "c", used::Used { model: used::Model::new("sonnet"), ..used::Used::default() });
    assert_eq!(name(&s), "claude sonnet");
    s.used(1, "c", used::Used { tokens: Some(5), ..used::Used::default() });
    assert_eq!(name(&s), "claude sonnet", "a read that names none keeps it");
    let got = strips(&s, &mut Memory::default(), &obs);
    assert_eq!(got[0].rows[0].model, "sonnet");
    assert_eq!(got[0].rows[0].task, "first line", "a given name's first line");
    assert_eq!(got[0].rows[1].task.chars().count(), 120, "and no more than a title has");
    dir_gone(&d);
}

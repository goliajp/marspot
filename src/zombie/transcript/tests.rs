use super::super::records::{Note, Rec, ToolUse};
use super::*;

fn bash(id: &str, background: bool, timeout_ms: Option<u64>) -> ToolUse {
    ToolUse { id: id.into(), name: "Bash".into(), background, command: Some(format!("cmd {id}")), timeout_ms }
}

fn note(task: &str, ended: bool, waiting: bool) -> Note {
    Note { task: task.into(), ended, waiting }
}

#[test]
fn zombie_main_log_follows_turns_tasks_and_their_ends() {
    let mut t = Tasks::default();
    let mut m = MainLog::default();
    m.feed(&Rec::Assistant { at: 10, uses: vec![bash("c1", true, None)], end: false }, &mut t);
    assert_eq!(m.turn_ended, None);
    m.feed(&Rec::User { at: 11, results: vec![("c1".into(), Some("b1".into()))], notes: vec![] }, &mut t);
    let rec = t.started.get(&"b1".to_string()).unwrap();
    assert!(rec.background);
    assert_eq!(rec.command.as_deref(), Some("cmd c1"));
    m.feed(&Rec::TurnEnd { at: 12 }, &mut t);
    assert_eq!(m.turn_ended, Some(12));
    m.feed(&Rec::Queued { at: 20, notes: vec![note("b1", true, false), note("x", false, false)] }, &mut t);
    assert_eq!(m.turn_ended, Some(12), "a queued notification does not start a turn");
    assert_eq!(t.ended.get(&"b1".to_string()), Some(&20));
    assert_eq!(t.ended.get(&"x".to_string()), None, "an event without a status ends nothing");
    m.feed(&Rec::User { at: 21, results: vec![], notes: vec![note("b1", true, false)] }, &mut t);
    assert_eq!(t.ended.get(&"b1".to_string()), Some(&20), "the first notification counts");
    assert_eq!(m.turn_ended, None, "handing it over starts a turn");
}

#[test]
fn zombie_sub_log_tells_a_finished_turn_from_an_open_call() {
    let mut t = Tasks::default();
    let mut s = SubLog::born(Some(5));
    s.feed(&Rec::Assistant { at: 10, uses: vec![], end: false }, &mut t);
    assert_eq!(s.last, Last::Busy, "thinking before a call is not the end of a turn");
    s.feed(&Rec::Assistant { at: 11, uses: vec![bash("c1", false, Some(600_000))], end: false }, &mut t);
    assert_eq!(s.last, Last::Bash { at: 11, timeout_ms: Some(600_000) });
    s.feed(&Rec::User { at: 12, results: vec![("c1".into(), None)], notes: vec![] }, &mut t);
    assert_eq!(s.last, Last::Busy);
    s.feed(&Rec::Assistant { at: 13, uses: vec![bash("c2", true, None)], end: false }, &mut t);
    assert_eq!(s.last, Last::Busy, "a background call is not waited on");
    s.feed(&Rec::User { at: 14, results: vec![("c2".into(), Some("b9".into()))], notes: vec![] }, &mut t);
    s.feed(&Rec::Assistant { at: 15, uses: vec![], end: true }, &mut t);
    assert_eq!(s.last, Last::Idle);
    assert_eq!((s.first, s.last_at), (Some(5), 15));
    let v = s.view("a1", false, |task| task == "b9");
    assert_eq!(v.tasks_alive, 1);
    assert_eq!(s.view("a1", false, |_| false).tasks_alive, 0);
}

#[test]
fn zombie_sub_log_with_a_call_still_open_is_not_idle_at_its_reply() {
    let mut t = Tasks::default();
    let mut s = SubLog::default();
    s.feed(&Rec::Assistant { at: 1, uses: vec![bash("c1", false, None), bash("c2", false, None)], end: false }, &mut t);
    s.feed(&Rec::User { at: 2, results: vec![("c1".into(), None)], notes: vec![] }, &mut t);
    s.feed(&Rec::Assistant { at: 3, uses: vec![], end: true }, &mut t);
    assert_eq!(s.last, Last::Busy, "c2 has no result yet");
}

#[test]
fn zombie_waiting_subagent_is_not_finished_and_a_finished_one_is() {
    let mut t = Tasks::default();
    t.notes(5, &[note("a1", true, true)]);
    assert!(t.finished.get(&"a1".to_string()).is_none());
    t.notes(6, &[note("a1", true, false)]);
    assert_eq!(t.finished.get(&"a1".to_string()), Some(&6));
}

#[test]
fn zombie_bounded_maps_forget_the_oldest() {
    let mut b = Bounded::new(3);
    for i in 0..5 {
        b.insert(i, i);
    }
    assert_eq!(b.len(), 3);
    assert!(b.get(&0).is_none() && b.get(&1).is_none() && b.get(&4).is_some());
    b.first(4, 99);
    assert_eq!(b.get(&4), Some(&4));
}

#[test]
fn zombie_meta_reads_description_depth_parent_and_finished() {
    let m = SubMeta::parse(r#"{"description":"Step 3b","spawnDepth":2,"parentAgentId":"aaf0","finished":true}"#);
    assert_eq!(m, SubMeta { description: "Step 3b".into(), depth: 2, parent: Some("aaf0".into()), finished: true });
    assert!(!SubMeta::parse(r#"{"description":"x"}"#).finished);
}

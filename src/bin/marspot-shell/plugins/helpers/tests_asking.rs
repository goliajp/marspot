//! A claude helper's approval prompt, by its record and its screen.

use std::cell::Cell;
use std::collections::HashMap;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::{Asked, asks};
use crate::plugins::claudecode::with_helpers::turns::{self, Look};
use crate::plugins::helpers::notify::{self, Stop};
use crate::plugins::helpers::table::{Event, Helper};
use crate::plugins::helpers::tests::{feed, host_with, shared_at, started, temp};
use crate::plugins::helpers::turn::Turn;
use crate::plugins::helpers::work::{self, Job};
use crate::plugins::helpers::{records, tick};

/// What the stand-in and claude draw at a prompt.
const ASKS: &str = "claude still working\n\n Bash command\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n";
const CLAUDE_ASKS: &str = "● Ran the tests\n\n────────────────────────────────────────\n Bash command\n\n   make clean\n   Remove the build\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again for make commands in /w\n   3. No, and tell Claude what to do differently (esc)\n\n Esc to cancel · Tab to amend\n";
/// Claude's AskUserQuestion: options with a line under each, and the
/// last one under a rule.
const QUESTION: &str = "● One choice before I go on.\n\n────────────────────────────────────────\n ☐ Cache\n\nWhich eviction should the cache use?\n\n❯ 1. LRU\n     Drop what was used longest ago\n  2. TTL\n     Drop entries after a fixed time\n  3. Type something.\n────────────────────────────────────────\n  4. Chat about this\n\nEnter to select · ↑/↓ to navigate · Esc to cancel\n";
/// A plan waiting for approval.
const PLAN: &str = "● Here is the plan.\n\n Would you like to proceed?\n\n ❯ 1. Yes, and auto-accept edits\n   2. Yes, and manually approve edits\n   3. No, keep planning\n\n ctrl-g to edit in Vim\n";
/// A tool's numbered output right above claude's input box.
const LISTED: &str = "● Bash(cat steps)\n  ⎿  1. fetch\n     2. build\n     3. ship\n\n✻ Thinking… (3s · esc to interrupt)\n\n────────────────────────────────────────\n❯ \n────────────────────────────────────────\n  ? for shortcuts\n";

#[test]
fn helpers_a_prompt_has_numbered_choices_at_the_bottom_of_its_screen() {
    assert!(notify::choices_at_bottom(ASKS));
    assert!(notify::choices_at_bottom(CLAUDE_ASKS));
    assert!(notify::choices_at_bottom(&format!("{CLAUDE_ASKS}\n\n\n")), "blank rows under it");
    let list_far_above = format!("Plan:\n 1. read\n 2. fix\n{}", "  working on it\n".repeat(12));
    assert!(!notify::choices_at_bottom(&list_far_above));
    assert!(!notify::choices_at_bottom("✻ Thinking… (12s · esc to interrupt)\n────\n❯ \n────\n"));
    for screen in [ASKS, CLAUDE_ASKS, QUESTION, PLAN] {
        assert!(asks(false, screen), "{screen}");
    }
    // numbered rows at the bottom, but the input box is there
    assert!(notify::choices_at_bottom(LISTED));
    assert!(!asks(true, LISTED));
}

#[test]
fn helpers_a_screen_is_read_on_the_worker_once_a_tool_call_is_drawn() {
    let dir = temp("asking");
    let (s, _rx) = shared_at(&dir);
    let queued = Cell::new(0);
    let goes = Cell::new(true);
    let at = |look: Option<Look>, now: u64, drawn: Option<u64>| {
        s.at_prompt(1, look, now, drawn, |_| {
            queued.set(queued.get() + 1);
            goes.get()
        })
    };
    let since = 1_000_000;
    let asking = Some(Look::Asking { since, question: false });
    assert!(!at(asking, since + 500, None), "not drawn yet");
    assert_eq!(queued.get(), 0);
    goes.set(false);
    assert!(!at(asking, since + 1_000, None));
    assert!(!at(asking, since + 1_100, None), "a read that did not go is asked for again");
    assert_eq!(queued.get(), 2);
    goes.set(true);
    assert!(!at(asking, since + 1_200, None));
    assert!(!at(asking, since + 1_500, None), "one read at a time");
    assert_eq!(queued.get(), 3);
    s.prompt_read(1, since, false, since + 1_600);
    assert!(!at(asking, since + 2_500, None), "read a moment ago");
    assert!(!at(asking, since + 3_600, None));
    assert_eq!(queued.get(), 4);
    s.prompt_read(1, since - 1, true, since + 3_650);
    assert!(!at(asking, since + 3_660, None), "a read for another call says nothing");
    s.prompt_read(1, since, true, since + 3_700);
    assert!(at(asking, since + 4_000, None));
    assert!(at(asking, since + 8_000, Some(since + 3_000)), "drawn before it was read");
    assert_eq!(queued.get(), 4, "and not read again for a while once it asked");
    assert!(at(asking, since + 8_700, None), "read again, and as it was until then");
    assert_eq!(queued.get(), 5);
    // approved: the tool runs in claude itself, and its box is back
    s.prompt_read(1, since, false, since + 8_800);
    assert!(!at(asking, since + 8_900, None));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_read_done_before_it_is_handed_over_leaves_none_under_way() {
    let dir = temp("asksync");
    let (s, _rx) = shared_at(&dir);
    let queued = Cell::new(0);
    let call = Some(Look::Asking { since: 1_000, question: false });
    // the worker reads it and reports before the queueing returns
    let at = |now: u64| {
        s.at_prompt(1, call, now, None, |since| {
            queued.set(queued.get() + 1);
            s.prompt_read(1, since, false, now);
            true
        })
    };
    assert!(!at(2_000));
    assert!(!s.lock().asking[&1].reading, "the read is in");
    assert!(!at(2_000 + super::REREAD_MS));
    assert_eq!(queued.get(), 2, "read again when due");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_prompt_seen_counts_no_more_once_the_pane_printed() {
    let dir = temp("askdrawn");
    let (s, _rx) = shared_at(&dir);
    let queued = Cell::new(0);
    let at = |now: u64, drawn: Option<u64>| {
        s.at_prompt(1, Some(Look::Asking { since: 1_000, question: false }), now, drawn, |_| {
            queued.set(queued.get() + 1);
            true
        })
    };
    assert!(!at(2_000, None));
    s.prompt_read(1, 1_000, true, 2_100);
    assert!(at(2_200, Some(1_900)));
    assert!(!at(2_400, Some(2_300)), "printed since it was read");
    assert_eq!(queued.get(), 2, "read again at once");
    assert!(!at(2_500, Some(2_300)), "until that read is in");
    s.prompt_read(1, 1_000, true, 2_600);
    assert!(at(2_700, Some(2_300)), "still the prompt");
    s.prompt_read(1, 1_000, false, 7_700);
    assert!(!at(7_800, Some(2_300)), "answered");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_call_that_ends_is_answered_and_a_scan_missed_is_not() {
    let dir = temp("askends");
    let (s, _rx) = shared_at(&dir);
    let at = |look: Option<Look>, now: u64| s.at_prompt(1, look, now, None, |_| true);
    let answered = || s.lock().answered.contains(&1);
    let call = |since: u64| Some(Look::Asking { since, question: false });
    at(call(1_000), 2_000);
    assert!(!at(None, 3_000), "no recent scan");
    assert!(!answered() && s.lock().asking.contains_key(&1), "says nothing of the call");
    at(call(70_000), 71_000);
    assert!(answered(), "another call");
    s.lock().answered.clear();
    at(Some(Look::NotOver), 72_000);
    assert!(answered(), "another record");
    assert!(s.lock().asking.is_empty(), "a call answered is forgotten");
    s.lock().answered.clear();
    at(Some(Look::NotOver), 73_000);
    assert!(!answered(), "with no call open, none was answered");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_prompt_on_the_screen_is_a_stop_before_the_pane_falls_quiet() {
    let dir = temp("askstop");
    let (s, rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]);
    let (mut in_turn, mut parked) = (HashMap::new(), tick::Parked::default());
    let busy = host_with(&[(1, PaneStatus::Busy(BusyKind::Output))]);
    let blocked = |rx: &std::sync::mpsc::Receiver<Job>| rx.try_iter().filter(|j| matches!(j, Job::Stopped { sid: 1, stop: Stop::Blocked, .. })).count();
    let since = records::now_ms() - 2_000;
    turns::publish(vec![(1, Look::Asking { since, question: false })]);
    // its screen, as read: no box, choices at the bottom
    s.lock().asking.insert(1, Asked { since, shown: true, next_ms: u64::MAX, ..Asked::default() });
    tick::tick(&s, &busy, &mut in_turn, &mut parked);
    assert_eq!((blocked(&rx), in_turn[&1]), (1, Turn::BlockedTold), "told at once");
    for _ in 0..3 {
        tick::tick(&s, &busy, &mut in_turn, &mut parked);
    }
    let awaiting = host_with(&[(1, PaneStatus::Busy(BusyKind::ToolAwaitingApproval))]);
    tick::tick(&s, &awaiting, &mut in_turn, &mut parked);
    assert_eq!(blocked(&rx), 0, "and once, also when the pane's state says so");
    // answered: the next record follows the call
    turns::publish(vec![(1, Look::NotOver)]);
    tick::tick(&s, &busy, &mut in_turn, &mut parked);
    assert_eq!(in_turn[&1], Turn::Working);
    turns::publish(Vec::new());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_prompt_told_of_is_told_again_only_at_another_call() {
    let dir = temp("asktwice");
    let (s, _rx) = shared_at(&dir);
    let begun = vec![Event::started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered];
    feed(&s, 1, [begun, vec![Event::BlockedNoticed { prompt: notify::prompt_sha(ASKS) }]].concat());
    super::super::give_up::LOOK.with(|l| *l.borrow_mut() = Some(std::sync::Arc::new(super::super::tests_review::panes(&[(10, 55)]))));
    let told = |screen: Option<&str>, by_screen: bool| {
        work::stopped_at(&s, 1, Stop::Blocked, by_screen, || screen.map(String::from));
        let h: Helper = s.table().map[&1].clone();
        s.lock().undelivered.take(1).is_some() || h.suppressed > 0
    };
    let at = |look: Look, now: u64| s.at_prompt(1, Some(look), now, None, |_| true);
    at(Look::Asking { since: 1_000, question: false }, 2_000);
    assert!(!told(Some(ASKS), false), "the same prompt, by the pane's state");
    assert!(!told(Some(ASKS), true), "or by the screen");
    assert!(told(None, false), "a screen not read may be another prompt");
    assert!(told(Some(CLAUDE_ASKS), false), "another prompt");
    // answered in the pane, and the same command tried again
    at(Look::NotOver, 3_000);
    at(Look::Asking { since: 4_000, question: false }, 5_000);
    assert!(told(Some(ASKS), false), "the same prompt at the next call");
    assert!(told(Some(ASKS), false), "until it is told");
    feed(&s, 1, vec![Event::BlockedNoticed { prompt: notify::prompt_sha(ASKS) }]);
    assert!(!told(Some(ASKS), false), "told: the call it is at is the one told of");
    // the next call recorded with no scan between
    at(Look::Asking { since: 6_000, question: false }, 7_000);
    assert!(told(Some(ASKS), false));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_a_process_under_claude_at_its_prompt_answers_nothing() {
    let dir = temp("askhook");
    let (s, _rx) = shared_at(&dir);
    feed(&s, 1, vec![Event::started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]);
    super::super::give_up::LOOK.with(|l| *l.borrow_mut() = Some(std::sync::Arc::new(super::super::tests_review::panes(&[(10, 55)]))));
    let notices = Cell::new(0);
    // told, or held back only by how recently the last notice went
    let blocked = || {
        let held = s.table().map[&1].suppressed;
        work::stopped_at(&s, 1, Stop::Blocked, false, || Some(ASKS.to_string()));
        if s.lock().undelivered.take(1).is_some() || s.table().map[&1].suppressed > held {
            notices.set(notices.get() + 1);
            feed(&s, 1, vec![Event::BlockedNoticed { prompt: notify::prompt_sha(ASKS) }]);
        }
    };
    let at = |look: Look, now: u64| s.at_prompt(1, Some(look), now, None, |_| true);
    at(Look::Asking { since: 1_000, question: false }, 2_000);
    blocked();
    // a Notification hook starts under claude, still at the prompt
    at(Look::Running { since: 1_000 }, 3_000);
    assert!(!s.lock().answered.contains(&1), "the call is still open");
    at(Look::Asking { since: 1_000, question: false }, 4_000);
    blocked();
    assert_eq!(notices.get(), 1, "one prompt, one notice");
    // approved: its tool ran, and the next call asks
    at(Look::Running { since: 1_000 }, 5_000);
    at(Look::Asking { since: 6_000, question: false }, 7_000);
    blocked();
    assert_eq!(notices.get(), 2, "another call");
    at(Look::Running { since: 8_000 }, 9_000);
    assert!(s.lock().answered.contains(&1) && s.lock().asking.is_empty(), "a process at the next call answered it");
    std::fs::remove_dir_all(&dir).unwrap();
}

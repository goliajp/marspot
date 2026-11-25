//! What is waited for after a stop that was told, and what is told
//! again after a start.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;

use super::notify::Stop;
use super::table::{Event, Helper};
use super::tests::{feed, host_with, shared_at, started, temp};
use super::turn::{self, AFTER_HELPERS_MS, Turn};
use super::work::Job;
use super::{records, rebuild, tick};
use crate::plugins::claudecode::with_helpers::turns;

fn stops(rx: &Receiver<Job>) -> Vec<(u64, Stop)> {
    rx.try_iter()
        .filter_map(|j| match j {
            Job::Stopped { sid, stop, .. } => Some((sid, stop)),
            _ => None,
        })
        .collect()
}

#[test]
fn helpers_one_given_keys_that_open_no_turn_is_not_waited_for() {
    let dir = temp("keys");
    let (s, rx) = shared_at(&dir);
    let begun = |parent| vec![Event::started(started(parent, "claude", AgentKind::Claude)), Event::TaskDelivered];
    // 1's own helper 2 stopped long ago, and 1 was told
    feed(&s, 1, begun(10));
    let now = records::now_ms();
    let child = Helper {
        delivered_ms: Some(now - 90_000),
        notices: vec![now - 60_000],
        noticed: true,
        ..Helper::new(2, started(1, "claude", AgentKind::Claude), now - 90_000)
    };
    feed(&s, 2, vec![Event::Carried(Box::new(child))]);
    let over = turns::Look::Over { began: None, at: now + 60_000, quiet_after: None };
    turns::publish(vec![(1, over)]);
    let mut in_turn = HashMap::new();
    let mut tick_at = |at: u64, child: PaneStatus| {
        let host = host_with(&[(1, PaneStatus::AwaitingUser), (2, child)]);
        for sid in [2, 1] {
            turn::turn(&s, &host, &s.table(), &s.table().map[&sid], at, &mut in_turn);
        }
        (in_turn[&1], stops(&rx))
    };
    s.input_seen(2);
    assert_eq!(tick_at(now, PaneStatus::AwaitingUser), (Turn::Held, vec![]), "its turn may be about to start");
    let later = now + AFTER_HELPERS_MS + 1_000;
    assert_eq!(tick_at(later, PaneStatus::AwaitingUser), (Turn::Idle, vec![(1, Stop::Idle)]));
    // a turn it did start is waited for, however long
    turns::publish(vec![(1, turns::Look::Over { began: None, at: now + 90_000, quiet_after: None })]);
    tick_at(later, PaneStatus::Busy(BusyKind::Working));
    assert_eq!(tick_at(now + 600_000, PaneStatus::Busy(BusyKind::Working)), (Turn::Held, vec![]));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_one_given_keys_before_a_start_is_not_waited_for_either() {
    let dir = temp("keysstart");
    let (s, _rx) = shared_at(&dir);
    for agent in [AgentKind::Claude, AgentKind::Kimi, AgentKind::Codex] {
        feed(&s, agent as u64 + 1, vec![Event::started(started(9, "claude", agent)), Event::TaskDelivered, Event::IdleNoticed, Event::InputSeen]);
    }
    feed(&s, 20, vec![Event::started(started(9, "claude", AgentKind::Claude)), Event::TaskDelivered]);
    s.lock().poked.clear();
    rebuild::rebuild(&s, || s.table().map.keys().copied().collect());
    let mut poked: Vec<u64> = s.lock().poked.keys().copied().collect();
    poked.sort();
    let mut want = vec![AgentKind::Claude as u64 + 1, AgentKind::Kimi as u64 + 1];
    want.sort();
    assert_eq!(poked, want, "codex is judged from its screen; 20 is at its first task");
    s.park();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn helpers_an_approval_told_before_a_start_is_not_told_after_it() {
    for agent in [AgentKind::Claude, AgentKind::Kimi] {
        let dir = temp("blocked");
        let (s, rx) = shared_at(&dir);
        feed(&s, 1, vec![Event::started(started(10, "shell", agent)), Event::TaskDelivered, Event::BlockedNoticed { prompt: String::new() }]);
        // a rotation between the two keeps it
        let carried = Event::Carried(Box::new(s.table().map[&1].clone()));
        let (_, _, back) = records::parse(&records::line(1, 1, 1, &carried)).unwrap();
        assert_eq!(back, carried);
        let mut in_turn = turn::turns_at_start(&s.table());
        let mut see = |status: PaneStatus| {
            for _ in 0..3 {
                tick::tick(&s, &host_with(&[(1, status.clone())]), &mut in_turn, &mut tick::Parked::default());
            }
            stops(&rx)
        };
        let asking = PaneStatus::Busy(BusyKind::ToolAwaitingApproval);
        assert_eq!(see(asking.clone()), vec![]);
        assert_eq!(see(PaneStatus::Busy(BusyKind::Working)), vec![]);
        assert_eq!(see(asking.clone()), vec![(1, Stop::Blocked)], "the next one is told");
        // answered through the socket: told again when it asks again
        feed(&s, 1, vec![Event::InputSeen]);
        assert_eq!(turn::turns_at_start(&s.table()).get(&1), Some(&Turn::Working));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

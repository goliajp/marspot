use marspot::settings::AgentAllocation;

use super::accounts::rooms;
use super::alloc::{Decision, KimiAlloc, PaneIn, Snap, quants};
use super::alloc_wire::{must_move_to, rooms_for};
use super::extra::{self, Extras, Shut};
use crate::plugins::allocation::replay::{Inputs, Why};
use crate::plugins::claudecode::quota::{Move, Refusal, Room, Situation, Window, next_move};
use crate::plugins::claudecode::seen::Seen;

mod rules;

const NOW: i64 = 1_794_290_114;
const MONTH_END: i64 = NOW + 20 * 86_400;

/// One account row of the kimi feed: five hours, week, month, wallet.
struct Row {
    n: u8,
    week: f64,
    reset_7d: i64,
    month: Option<f64>,
    wallet: Option<f64>,
}

fn row(n: u8, week: f64, days: i64) -> Row {
    Row { n, week, reset_7d: NOW + days * 86_400, month: None, wallet: None }
}

fn feed(rows: &[Row]) -> String {
    let rows: Vec<String> = rows
        .iter()
        .map(|r| {
            let month = r.month.map_or_else(String::new, |m| format!(r#","utilization_month":{m},"reset_month":{MONTH_END}"#));
            let credits = r.wallet.map_or_else(|| "null".to_string(), |w| format!(r#"{{"enabled":true,"balance":{w},"limit":5000000}}"#));
            let name = if r.n == 0 { "Kimi".to_string() } else { format!("Kimi {}", r.n) };
            let status = if r.week >= 1.0 { "rejected" } else { "allowed" };
            format!(
                r#"{{"name":"{name}","email":"","status":"{status}","utilization_5h":0.1,"utilization_7d":{},"reset_5h":{},"reset_7d":{}{month},"model_limits":[],"credits":{credits},"tier":"","collected_at":"2026-11-10T05:50:00+00:00"}}"#,
                r.week,
                NOW + 3_600,
                r.reset_7d
            )
        })
        .collect();
    format!(r#"{{"generated_at":"2026-11-10T05:55:14+00:00","accounts":[{}]}}"#, rows.join(","))
}

fn situation(rooms: &[(u8, Room)], current: u8) -> Situation<'_> {
    Situation {
        rooms,
        current,
        tool_executing: false,
        work_running: false,
        moves_this_episode: 0,
        since_last_move: None,
        stranded_for: None,
        warned: false,
        last_target: None,
        needs_model: None,
        background_refused: None,
    }
}

/// P1 refused a pane; P2 renews first but its month is spent; P3, P4 have room.
fn scene(p2: Row) -> (String, Seen) {
    let text = feed(&[row(1, 0.3, 4), p2, row(3, 0.5, 3), row(4, 0.6, 5)]);
    let mut seen = Seen::default();
    seen.note(1, &Refusal { at: NOW - 30, model: String::new(), logged_out: false, window: Window::FiveHour });
    (text, seen)
}

fn view(mode: AgentAllocation, text: &str, seen: &Seen, now: i64) -> Vec<(u8, Room)> {
    let usage = marspot::cc_usage::parse(text).unwrap();
    rooms_for(mode, rooms(&[1, 2, 3, 4], Some(&usage), seen, now), &extra::parse(text), now)
}

#[test]
fn kimi_a_monthly_window_and_a_wallet_are_read_from_the_feed() {
    let text = feed(&[Row { month: Some(1.0), wallet: Some(2_500_000.0), ..row(2, 0.4, 2) }, row(3, 0.5, 3)]);
    let extras = extra::parse(&text);
    let two = &extras[&2];
    assert_eq!((two.month, two.wallet_left), (Some((1.0, Some(MONTH_END))), Some(2_500_000.0)));
    assert_eq!(two.shut(NOW), Some(Shut::MonthSpent));
    assert_eq!(two.shut(MONTH_END), None, "the month renewed");
    assert_eq!(extras[&3].shut(NOW), None);
    assert!(extra::parse("not json").is_empty());
}

#[test]
fn kimi_an_account_with_its_month_spent_is_no_target_until_the_month_renews() {
    let (text, seen) = scene(Row { month: Some(1.0), ..row(2, 0.2, 2) });
    // today's rule does not read the month and sends the refused pane to P2
    let today = view(AgentAllocation::Off, &text, &seen, NOW);
    assert_eq!(next_move(&situation(&today, 1)), Move::To(2));
    let live = view(AgentAllocation::Live, &text, &seen, NOW);
    assert!(live[1].1.refused_for(None));
    let to = next_move(&situation(&live, 1));
    assert!(matches!(to, Move::To(n) if n != 2), "{to:?}");
    let q = quants(&live, &extra::parse(&text), NOW);
    assert!(!q.iter().find(|q| q.id == 2).unwrap().open, "the core does not open it either");
    // the month renews: P2 is the first choice again
    let later = NOW + 20 * 86_400 + 1;
    let seen_later = Seen::default();
    let renewed = Row { month: Some(1.0), ..Row { reset_7d: later + 86_400, ..row(2, 0.2, 0) } };
    let text = feed(&[row(1, 1.0, 30), renewed, row(3, 0.5, 30), row(4, 0.6, 30)]);
    assert_eq!(next_move(&situation(&view(AgentAllocation::Live, &text, &seen_later, later), 1)), Move::To(2));
}

#[test]
fn kimi_an_account_paying_beyond_its_allowance_is_no_target() {
    let paying = Row { week: 1.0, wallet: Some(1_000.0), ..row(2, 0.0, 2) };
    let extras = extra::parse(&feed(&[Row { week: 1.0, wallet: Some(1_000.0), ..row(2, 0.0, 2) }]));
    assert_eq!(extras[&2].shut(NOW), Some(Shut::Paying));
    let spent_wallet = extra::parse(&feed(&[Row { week: 1.0, wallet: Some(0.0), ..row(2, 0.0, 2) }]));
    assert_eq!(spent_wallet[&2].shut(NOW), None, "an empty wallet pays nothing; the full week shuts it as today");
    let (text, seen) = scene(paying);
    for mode in [AgentAllocation::Off, AgentAllocation::DryRun, AgentAllocation::Live] {
        let r = view(mode, &text, &seen, NOW);
        assert!(r[1].1.refused_for(None), "{mode:?}");
        assert!(matches!(next_move(&situation(&r, 1)), Move::To(n) if n != 2), "{mode:?}");
    }
    let mut rooms = view(AgentAllocation::Off, &text, &seen, NOW);
    assert_eq!(extra::mark(&mut rooms, &extra::parse(&text), NOW), vec![(2, Shut::Paying)]);
}

fn decision(to: u8, why: Why) -> Decision {
    let inputs = Inputs { accounts: Vec::new(), b_f: 0.0, b_n: 0.0, kappa: None, loads: Vec::new(), mover: crate::plugins::allocation::choose::Mover { class: crate::plugins::allocation::Class::Plain, rate: 0.0, tokens: None }, here: 1, kind: crate::plugins::allocation::replay::Kind::Out { horizon_h: 6.0 } };
    Decision { sid: 7, to, why, inputs }
}

#[test]
fn kimi_off_keeps_todays_decision() {
    let (text, seen) = scene(Row { month: Some(1.0), wallet: Some(9.0), ..row(2, 0.2, 2) });
    let usage = marspot::cc_usage::parse(&text).unwrap();
    let today = rooms(&[1, 2, 3, 4], Some(&usage), &seen, NOW);
    for mode in [AgentAllocation::Off, AgentAllocation::DryRun] {
        assert_eq!(view(mode, &text, &seen, NOW), today, "{mode:?} decides on today's accounts");
        let m = next_move(&situation(&today, 1));
        assert_eq!(m, Move::To(2));
        assert_eq!(must_move_to(mode, m, Some(&decision(3, Why::Out)), 1, false), Move::To(2), "{mode:?}");
    }
    // live, an account the readings say is spent: the core's target
    assert_eq!(must_move_to(AgentAllocation::Live, Move::To(2), Some(&decision(3, Why::Out)), 1, false), Move::To(3));
    assert_eq!(must_move_to(AgentAllocation::Live, Move::Hold("x"), Some(&decision(3, Why::Out)), 1, false), Move::Hold("x"), "today's holds stand");
    assert_eq!(must_move_to(AgentAllocation::Live, Move::To(2), Some(&decision(3, Why::Plan)), 1, false), Move::To(2));
}

fn would_lines(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(e.path()).unwrap();
        out.extend(text.lines().filter(|l| l.split('\t').nth(3) == Some("would_move")).map(str::to_string));
    }
    out
}

#[test]
fn kimi_dryrun_writes_one_would_move_per_change_and_each_replays_the_same() {
    let tmp = marspot_term::test_env::TempDir::new("kimi-alloc-dryrun");
    let mut alloc = KimiAlloc::new(tmp.path().to_path_buf(), false);
    let pass = |alloc: &mut KimiAlloc, text: &str, now: i64| {
        let usage = marspot::cc_usage::parse(text).unwrap();
        let (_, seen) = scene(row(2, 0.0, 2));
        let extras: Extras = extra::parse(text);
        let mut r = rooms(&[1, 2, 3, 4], Some(&usage), &seen, now);
        extra::mark(&mut r, &extras, now);
        let panes = vec![PaneIn { sid: 7, profile: 1, idle: true, helper: false, out: true }];
        let snap = Snap { now, feed_at: Some(usage.generated_at), rooms: &r, extras: &extras, panes };
        alloc.observe(&snap)
    };
    let (text, _) = scene(Row { month: Some(1.0), ..row(2, 0.2, 2) });
    let first = pass(&mut alloc, &text, NOW);
    assert_eq!(first.len(), 1);
    let target = first[0].to;
    assert!(target == 3 || target == 4, "never P2, whose month is spent, nor the refusing P1: {target}");
    assert_eq!(pass(&mut alloc, &text, NOW + 1), first, "same inputs");
    assert_eq!(pass(&mut alloc, &text, NOW + 120)[0].to, target, "same target a while later");
    assert_eq!(would_lines(tmp.path()).len(), 1);
    // the target's month is spent too: the decision changes once
    let other = 7 - target;
    let rows = |spent: u8| {
        let mut rs = vec![row(1, 0.3, 4), Row { month: Some(1.0), ..row(2, 0.2, 2) }, row(3, 0.5, 3), row(4, 0.6, 5)];
        rs[usize::from(spent) - 1].month = Some(1.0);
        feed(&rs)
    };
    let changed = pass(&mut alloc, &rows(target), NOW + 180);
    assert_eq!(changed[0].to, other);
    pass(&mut alloc, &rows(target), NOW + 300);
    let lines = would_lines(tmp.path());
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for line in &lines {
        let detail = line.split('\t').nth(6).unwrap();
        let to: u8 = detail.split(' ').find_map(|w| w.strip_prefix("to=P")).unwrap().parse().unwrap();
        let why = detail.split(' ').find_map(|w| w.strip_prefix("why=")).and_then(Why::from_word).unwrap();
        assert_eq!(crate::plugins::allocation::replay::replay(detail), Some(Some((to, why))), "{detail}");
        assert_eq!(line.split('\t').nth(1), Some("kimi"));
    }
    // a restart reads its lines back and writes no repeat
    let mut again = KimiAlloc::new(tmp.path().to_path_buf(), false);
    pass(&mut again, &rows(target), NOW + 400);
    assert_eq!(would_lines(tmp.path()).len(), 2);
    // live writes no would_move
    let mut live = KimiAlloc::new(tmp.path().join("live"), true);
    let usage = marspot::cc_usage::parse(&text).unwrap();
    let r = rooms(&[1, 2, 3, 4], Some(&usage), &Seen::default(), NOW);
    let panes = vec![PaneIn { sid: 7, profile: 1, idle: true, helper: false, out: true }];
    live.observe(&Snap { now: NOW, feed_at: Some(usage.generated_at), rooms: &r, extras: &Extras::new(), panes });
    assert!(would_lines(&tmp.path().join("live")).is_empty());
}

#[test]
fn kimi_a_refused_pane_moves_as_today_in_every_mode() {
    // no month and no wallet: every mode makes today's move
    let text = feed(&[row(1, 0.3, 4), row(2, 0.2, 2), row(3, 0.5, 3)]);
    let mut seen = Seen::default();
    seen.note(1, &Refusal { at: NOW - 30, model: String::new(), logged_out: false, window: Window::FiveHour });
    let usage = marspot::cc_usage::parse(&text).unwrap();
    let today = rooms(&[1, 2, 3], Some(&usage), &seen, NOW);
    assert_eq!(next_move(&situation(&today, 1)), Move::To(2));
    for mode in [AgentAllocation::Off, AgentAllocation::DryRun, AgentAllocation::Live] {
        let r = rooms_for(mode, rooms(&[1, 2, 3], Some(&usage), &seen, NOW), &extra::parse(&text), NOW);
        assert_eq!(r, today, "{mode:?}");
        assert_eq!(must_move_to(mode, next_move(&situation(&r, 1)), None, 1, true), Move::To(2), "{mode:?}");
        // the core would send it elsewhere; after a refusal today's target stands
        assert_eq!(must_move_to(mode, next_move(&situation(&r, 1)), Some(&decision(3, Why::Out)), 1, true), Move::To(2), "{mode:?}");
    }
    assert!(super::accounts::refused(Some(&usage), &seen, 1, NOW), "P1's refusal is what makes this move a refusal's");
    assert!(!super::accounts::refused(Some(&usage), &seen, 2, NOW));
    // the refusal outlives its pane's account only until its window renews
    assert!(!rooms(&[1], Some(&usage), &seen, NOW + 3_601)[0].1.refused_for(None));
}

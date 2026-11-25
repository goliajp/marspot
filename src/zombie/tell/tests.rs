use super::super::check::{Hit, Key, Suspect};
use super::super::rules::Rule;
use super::*;

const NOW: u64 = 1_800_000_000;

fn suspect(i: usize) -> Suspect {
    let key = Key { sid: 7, rule: Rule::B1, id: format!("{}@1", 100 + i) };
    Suspect { hit: Hit { key, evidence: format!("后台任务 b{i}，命令：{}", "x".repeat(200)) }, since: NOW }
}

#[test]
fn zombie_pacing_keeps_four_hours_per_pane_and_the_hourly_and_daily_caps() {
    let mut p = Pacer::default();
    assert!(p.allows(7, NOW));
    p.record(7, NOW, ["a".to_string()]);
    assert!(!p.allows(7, NOW + PANE_GAP - 1));
    assert!(p.allows(7, NOW + PANE_GAP));
    p.record(8, NOW + 1, []);
    p.record(9, NOW + 2, []);
    assert!(!p.allows(10, NOW + 3), "three this hour across the host");
    assert!(p.allows(10, NOW + HOUR));
    let mut day = Pacer::default();
    for i in 0..HOST_PER_DAY as u64 {
        day.record(100 + i, NOW + i * HOUR / 2, []);
    }
    assert!(!day.allows(999, NOW + 7 * HOUR), "twelve today");
    assert!(day.allows(999, NOW + 24 * HOUR + 1));
}

fn ready() -> Reach {
    Reach { turn_ended: true, awaiting_for: Some(TURN_OVER), user_here: false, refused: false, dormant: false }
}

#[test]
fn zombie_an_item_is_told_once_and_forgotten_when_gone() {
    let mut p = Pacer::default();
    let s = suspect(0);
    assert_eq!(due(&[&s], &p, Some(&ready()), 1), Ok(()));
    p.record(7, NOW, [s.hit.key.id.clone()]);
    assert_eq!(due(&[&s], &p, Some(&ready()), 1), Err("told"), "told already");
    let t = suspect(1);
    assert_eq!(due(&[&s, &t], &p, Some(&ready()), 1), Ok(()), "one item never told is enough");
    p.keep_alive(|id| id != s.hit.key.id);
    assert!(!p.told(&s.hit.key.id));
}

#[test]
fn zombie_a_message_waits_for_an_ended_turn_held_two_minutes() {
    let p = Pacer::default();
    let s = suspect(0);
    let r = |f: fn(&mut Reach)| {
        let mut r = ready();
        f(&mut r);
        r
    };
    assert_eq!(due(&[&s], &p, None, 1), Err("no_pane_state"));
    assert_eq!(due(&[&s], &p, Some(&r(|r| r.turn_ended = false)), 1), Err("turn_running"));
    assert_eq!(due(&[&s], &p, Some(&r(|r| r.awaiting_for = None)), 1), Err("not_waiting_long"));
    assert_eq!(due(&[&s], &p, Some(&r(|r| r.awaiting_for = Some(TURN_OVER - 1))), 1), Err("not_waiting_long"));
    assert_eq!(due(&[&s], &p, Some(&r(|r| r.awaiting_for = Some(TURN_OVER / 60))), 60), Ok(()), "scaled for a window test");
}

#[test]
fn zombie_a_message_waits_while_the_person_is_in_the_pane() {
    let mut r = ready();
    r.user_here = true;
    assert_eq!(due(&[&suspect(0)], &Pacer::default(), Some(&r), 1), Err("user_here"));
}

#[test]
fn zombie_no_message_to_a_refused_account_or_a_resting_claude() {
    let s = suspect(0);
    let mut r = ready();
    r.refused = true;
    assert_eq!(due(&[&s], &Pacer::default(), Some(&r), 1), Err("quota_refused"));
    let mut r = ready();
    r.dormant = true;
    assert_eq!(due(&[&s], &Pacer::default(), Some(&r), 1), Err("dormant"));
}

#[test]
fn zombie_pacing_names_the_limit_that_holds_a_message() {
    let mut p = Pacer::default();
    assert_eq!(p.holds(7, NOW), None);
    p.record(7, NOW, []);
    assert_eq!(p.holds(7, NOW + 1), Some("pane_gap"));
    p.record(7, NOW + PANE_GAP, []);
    p.record(7, NOW + 2 * PANE_GAP, []);
    assert_eq!(p.holds(7, NOW + 3 * PANE_GAP), Some("pane_day"), "three to one pane within a day");
    assert_eq!(p.holds(7, NOW + DAY), None, "the first of the three is a day old");
    let mut h = Pacer::default();
    for sid in 1..=3 {
        h.record(sid, NOW + sid, []);
    }
    assert_eq!(h.holds(9, NOW + 10), Some("host_hour"));
    let mut d = Pacer::default();
    for i in 0..HOST_PER_DAY as u64 {
        d.record(100 + i, NOW + i * HOUR, []);
    }
    assert_eq!(d.holds(9, NOW + 12 * HOUR), Some("host_day"));
}

#[test]
fn zombie_pacing_windows_shrink_with_the_scale() {
    let mut p = Pacer { scale: 60, ..Pacer::default() };
    p.record(7, NOW, []);
    assert_eq!(p.holds(7, NOW + PANE_GAP / 60 - 1), Some("pane_gap"));
    assert_eq!(p.holds(7, NOW + PANE_GAP / 60), None);
}

const HEAD: &str = "marspot 提醒：程序检查发现这个会话名下有";
const TAIL: &str = "不需要的：调用 ProcStop，ids 照抄上面的 pid@数字（会连同它下面的进程一起结束），或者用 TaskStop 加任务 id。\n不确定的：先读它的输出文件再决定。";

#[test]
fn zombie_message_is_the_designed_text() {
    let key = |rule, id: &str| Key { sid: 7, rule, id: id.to_string() };
    let b1 = Suspect {
        hit: Hit { key: key(Rule::B1, "36160@1795584627123456"), evidence: "后台任务 b6lpc8u81（pid 36160@1795584627123456），命令：bash watchdog.sh\n   已跑 8 小时 45 分".into() },
        since: NOW,
    };
    let o1 = Suspect {
        hit: Hit { key: key(Rule::O1, "5@6"), evidence: "被收养的 tail（pid 5@6）。它继承的环境变量 CLAUDE_CODE_SESSION_ID 是这个会话".into() },
        since: NOW,
    };
    let want = format!(
        "{HEAD} 2 项疑似已经没用的工作。marspot 不会自己结束它，请你核对后处理；处理完不用回复，不处理也不会再提醒同一项。\n\n\
         1. 后台任务 b6lpc8u81（pid 36160@1795584627123456），命令：bash watchdog.sh\n   已跑 8 小时 45 分\n\n\
         2. 被收养的 tail（pid 5@6）。它继承的环境变量 CLAUDE_CODE_SESSION_ID 是这个会话\n\n{TAIL}"
    );
    let (got, shown) = body(&[&b1, &o1]);
    assert_eq!(shown, 2);
    assert_eq!(got, want);
    assert!(!got.contains("请结束"));
}

#[test]
fn zombie_message_lists_at_most_twelve_items_then_counts_the_rest() {
    let short: Vec<Suspect> = (0..15).map(|i| Suspect { hit: Hit { evidence: format!("任务 b{i}"), ..suspect(i).hit }, since: NOW }).collect();
    let refs: Vec<&Suspect> = short.iter().collect();
    let (b, shown) = body(&refs);
    assert_eq!(shown, 12);
    assert!(b.contains("\n12. 任务 b11\n") && !b.contains("13. "), "{b}");
    assert!(b.contains("另有 3 项。\n\n") && b.ends_with(TAIL));
    let (exactly, _) = body(&refs[..12]);
    assert!(!exactly.contains("另有"));
}

#[test]
fn zombie_message_stays_within_4_kib() {
    let long: Vec<Suspect> = (0..20)
        .map(|i| {
            let mut s = suspect(i);
            s.hit.evidence = format!("后台任务 b{i}，命令：{}", "长".repeat(600));
            s
        })
        .collect();
    let refs: Vec<&Suspect> = long.iter().collect();
    let (b, shown) = body(&refs);
    assert!(b.contains(&format!("另有 {} 项", 20 - shown)), "{shown} listed");
    assert!(b.len() <= 4096, "{} bytes", b.len());
    assert!(b.starts_with(&format!("{HEAD} 20 项")));
    assert!(b.contains("另有") && b.ends_with(TAIL), "the cut items are counted and the closing lines stay");
}

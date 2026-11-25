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

#[test]
fn zombie_an_item_is_told_once_and_forgotten_when_gone() {
    let mut p = Pacer::default();
    let s = suspect(0);
    assert!(due(&[&s], &p, Some(NOW - TURN_OVER), NOW));
    assert!(!due(&[&s], &p, Some(NOW - TURN_OVER + 1), NOW), "the turn ended under two minutes ago");
    assert!(!due(&[&s], &p, None, NOW), "a turn is running");
    p.record(7, NOW, [s.hit.key.id.clone()]);
    assert!(!due(&[&s], &p, Some(0), NOW + PANE_GAP), "told already");
    p.keep_alive(|id| id != s.hit.key.id);
    assert!(!p.told(&s.hit.key.id));
}

#[test]
fn zombie_message_lists_at_most_twelve_items_within_4_kib() {
    let many: Vec<Suspect> = (0..20).map(suspect).collect();
    let refs: Vec<&Suspect> = many.iter().collect();
    let b = body(&refs);
    assert!(b.len() <= 4096, "{} bytes", b.len());
    assert!(b.starts_with("marspot 提醒：程序检查发现这个会话名下有 20 项"));
    assert!(b.contains("另有"), "the rest are counted");
    let one = body(&refs[..1]);
    assert!(one.contains("1. [B1] 后台任务 b0") && !one.contains("另有"));
}

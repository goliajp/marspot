use super::*;

/// 2026-08-11 的原形:核心启动慢,握手完成时 spawn 锚点已经过
/// 期,于是它刚说完 hello 就被判"卡死"。
#[test]
fn a_slow_boot_still_gets_a_full_window_to_answer() {
    let hello = Instant::now();
    // 旧行为:锚点在 spawn。握手花了 20 秒 → 握手当场就超期。
    let spawned = hello - Duration::from_secs(20);
    assert!(
        pong_overdue(true, spawned, hello),
        "spawn 锚点会让慢启动的核心在握手瞬间被判死 —— 这正是要修的",
    );
    // 新行为:锚点在 HelloAck。整整一个 PONG_DEADLINE 都归它。
    assert!(!pong_overdue(true, hello, hello + PONG_DEADLINE));
    assert!(pong_overdue(true, hello, hello + PONG_DEADLINE + Duration::from_secs(1)));
    // 还没握上手的核心归 HELLO_TIMEOUT 管,不归这里。
    assert!(!pong_overdue(false, spawned, hello));
}

/// 我们自己没在跑的那段时间,不能记在对方账上。
#[test]
fn time_we_did_not_witness_is_handed_back() {
    let now = Instant::now();
    let anchor = now - Duration::from_secs(60);
    // 整整 60 秒的停摆:锚点回到 now,立刻重新计时,不判超期。
    let forgiven = forgive_stall(anchor, Duration::from_secs(60), now);
    assert!(!pong_overdue(true, forgiven, now));
    // 但不许推到未来 —— 那等于白送一个 deadline。
    assert_eq!(forgive_stall(anchor, Duration::from_secs(600), now), now);
    // 停摆只有 1 秒,就只还 1 秒:真卡死仍然抓得到。
    let barely = forgive_stall(anchor, Duration::from_secs(1), now);
    assert!(pong_overdue(true, barely, now));
}

/// 冷却时间翻倍、封顶,且永远不为零 —— 为零就等于没有预算。
#[test]
fn the_cool_down_doubles_then_stops_doubling() {
    assert_eq!(restart_cooldown(0), CRASH_WINDOW);
    assert_eq!(restart_cooldown(1), CRASH_WINDOW);
    assert_eq!(restart_cooldown(2), CRASH_WINDOW * 2);
    assert_eq!(restart_cooldown(4), CRASH_WINDOW * 8);
    assert_eq!(restart_cooldown(99), CRASH_WINDOW * 8, "封顶后不再增长");
}

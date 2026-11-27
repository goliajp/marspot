//! A working run's badge ends with a mark that does not need a timer.

use super::*;

#[test]
fn a_working_runs_badge_is_one_static_mark_however_often_it_ticks() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.switch").badge("→ P5").step(Step::settle(Duration::from_secs(5)));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 3_000);
    let badges = host.badges.lock().unwrap();
    assert!(!badges.is_empty());
    assert!(badges.iter().all(|b| *b == format!("→ P5 {WORKING_MARK}")), "{badges:?}");
}

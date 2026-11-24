//! A wait on a noted job says so beside the badge, and only while it
//! waits.

use super::tests::{run, setup};
use super::*;

#[test]
fn the_badge_names_a_noted_wait_while_it_lasts() {
    let (state, env, host) = setup();
    let job = Job::noted("waiting for P1");
    let op = PtyOp::new("test.wait").badge("→ P2").step(Step::await_job(Arc::clone(&job))).step(Step::settle(Duration::from_secs(1)));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 500);
    let waiting = host.badges.lock().unwrap().drain(..).collect::<Vec<_>>();
    assert!(!waiting.is_empty() && waiting.iter().all(|b| b.starts_with("→ P2 waiting for P1 ")), "{waiting:?}");
    job.finish(Ok(None));
    run(&mut r, &host, &state, 500);
    let after = host.badges.lock().unwrap().clone();
    assert!(!after.is_empty() && after.iter().all(|b| !b.contains("waiting")), "{after:?}");
}

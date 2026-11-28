//! A run guarded by `untouched` types nothing into a pane that had
//! input since, or once its window has passed.

use std::time::{Duration, SystemTime};

use super::super::tests::{run, setup};
use super::super::{OpRunner, PtyOp, Step};

fn at(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

fn guarded(since: SystemTime, until: SystemTime) -> PtyOp {
    PtyOp::new("test.guarded").step(Step::untouched(since, until)).step(Step::send(b"claude\r".to_vec()))
}

#[test]
fn an_untouched_pane_in_its_window_is_typed_into() {
    let (state, env, host) = setup();
    *state.last_input.lock().unwrap() = Some(at(0));
    let mut r = OpRunner::new(guarded(at(1), at(60_000)), env);
    run(&mut r, &host, &state, 200);
    assert_eq!(*state.sent.lock().unwrap(), vec![b"claude\r".to_vec()], "input before `since` does not count");
}

#[test]
fn input_since_stops_the_run_before_it_types() {
    let (state, env, host) = setup();
    *state.last_input.lock().unwrap() = Some(at(5));
    let mut r = OpRunner::new(guarded(at(1), at(60_000)), env);
    run(&mut r, &host, &state, 200);
    assert!(state.sent.lock().unwrap().is_empty(), "half a typed command is left alone");
    assert!(host.logs.lock().unwrap().iter().any(|l| l.starts_with("test.guarded.not_typed") && l.contains("input since")));
}

#[test]
fn a_window_that_has_passed_stops_the_run() {
    let (state, env, host) = setup();
    let late = PtyOp::new("test.guarded").step(Step::settle(Duration::from_millis(50)));
    let late = guarded(at(0), at(10)).steps.into_iter().fold(late, PtyOp::step);
    let mut r = OpRunner::new(late, env);
    run(&mut r, &host, &state, 200);
    assert!(state.sent.lock().unwrap().is_empty());
}

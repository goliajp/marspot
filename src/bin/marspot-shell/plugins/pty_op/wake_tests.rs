//! Waking a run parked on the user: by focus, as the user does, and by
//! input an agent sends to the pane.

use super::*;
use marspot_peer::control::ErrorCode;

/// A run that exists to bring something back must not do it twice.
///
/// The gap between parking a session and typing it back holds the
/// user's own click, and in that gap the session can return by
/// other means — a second wake armed on the same pane, or the user
/// starting it themselves.  Typing then puts `claude --resume …`
/// into the running session's prompt, as text they have to delete.
/// Seen on the real machine twice in one day, which is what this
/// step is for.
#[test]
fn a_run_stops_when_what_it_was_arranging_has_already_happened() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake")
        .step(Step::await_user())
        .step(Step::stop_if_process(1, |_| true))
        .step(Step::send(b"resume\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert!(r.is_awaiting_user());

    // It came back on its own while we were parked.
    state.present.lock().unwrap().push(99);
    r.on_focus(&host);
    run(&mut r, &host, &state, 100);
    assert!(
        state.sent.lock().unwrap().is_empty(),
        "nothing to type — it is already back"
    );
    assert!(*host.ended.lock().unwrap(), "and the run is over, successfully");
}

/// …and when it has not, the run carries on as before.
#[test]
fn a_run_carries_on_when_nothing_has_come_back() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake")
        .step(Step::await_user())
        .step(Step::stop_if_process(1, |_| true))
        .step(Step::send(b"resume\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    r.on_focus(&host);
    run(&mut r, &host, &state, 100);
    assert_eq!(*state.sent.lock().unwrap(), vec![b"resume\r".to_vec()]);
}

/// An agent's input for a pane whose agent sleeps wakes it, as the user
/// coming back would, and is refused until the pane is up again; input
/// for a pane that is awake is let through and wakes nothing.
#[test]
fn input_from_an_agent_wakes_a_sleeping_pane() {
    use crate::pane_control::mainloop::wake_for_input;
    let (state, env, host) = setup();
    let op = PtyOp::new("test.reclaim")
        .step(Step::await_user())
        .step(Step::send(b"resume\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert!(PaneSession::parked(&r), "asleep");

    let refused = wake_for_input(&mut r, &host).expect("input waits while the pane wakes");
    assert_eq!(refused.code, ErrorCode::PaneDormant);
    assert!(refused.retry_after_ms.is_some_and(|ms| ms > 0), "the agent is told when to try again");
    assert!(!PaneSession::parked(&r), "woken by the input");
    run(&mut r, &host, &state, 100);
    assert_eq!(*state.sent.lock().unwrap(), vec![b"resume\r".to_vec()], "the resume line goes out");

    assert!(wake_for_input(&mut r, &host).is_none(), "awake: the input goes through");
    run(&mut r, &host, &state, 100);
    assert_eq!(state.sent.lock().unwrap().len(), 1, "and nothing is typed twice");
}

/// Input that wakes a pane is logged as input, not as the user's focus.
#[test]
fn a_wake_by_input_is_logged_as_input() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.reclaim").step(Step::await_user()).step(Step::send(b"resume\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert!(PaneSession::wake_for_input(&mut r, &host));
    let logs = host.logs.lock().unwrap();
    assert!(logs.iter().any(|l| l == "test.reclaim.woken pane 1 by input"), "{logs:?}");
}

/// While what a wake started still runs, a refused input is told when
/// to try again, never later than what is left of the wake's bound.
#[test]
fn input_refused_while_a_pane_loads_after_a_wake_is_told_when_to_retry() {
    use crate::pane_control::mainloop::{busy, wake_for_input};
    let (state, env, host) = setup();
    let op = PtyOp::new("test.reclaim")
        .wake_bound(Duration::from_secs(30))
        .step(Step::await_user())
        .step(Step::settle(Duration::from_secs(60)));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert_eq!(PaneSession::waking_for(&r), None, "not woken yet");

    let dormant = wake_for_input(&mut r, &host).expect("refused while it wakes");
    assert_eq!(dormant.retry_after_ms, Some(3_000));
    assert!(dormant.message.contains("up to 30 s"), "{}", dormant.message);
    run(&mut r, &host, &state, 3_200);
    let left = PaneSession::waking_for(&r).expect("still loading");
    assert!(left < Duration::from_secs(27) && left > Duration::from_secs(26), "{left:?}");
    let b = busy("test.reclaim", Some(left));
    assert_eq!((b.code, b.retry_after_ms), (ErrorCode::PaneBusy, Some(3_000)));

    run(&mut r, &host, &state, 26_600);
    let left = PaneSession::waking_for(&r).expect("still running");
    assert_eq!(busy("test.reclaim", Some(left)).retry_after_ms, Some(500), "{left:?}");
    run(&mut r, &host, &state, 2_000);
    assert_eq!(PaneSession::waking_for(&r), Some(Duration::ZERO), "past the bound");
    assert_eq!(busy("x", None).retry_after_ms, None, "a script that is no wake has no time to give");
}

/// A session that stays parked when input asks for its pane says so,
/// and promises no retry.
#[test]
fn input_for_a_pane_that_cannot_be_woken_promises_no_retry() {
    use crate::pane_control::mainloop::wake_for_input;
    struct Asleep;
    impl PaneSession for Asleep {
        fn caps(&self) -> u32 {
            0
        }
        fn parked(&self) -> bool {
            true
        }
    }
    let (_state, _env, host) = setup();
    let r = wake_for_input(&mut Asleep, &host).expect("refused");
    assert_eq!((r.code, r.retry_after_ms), (ErrorCode::PaneDormant, None));
    assert!(!r.message.contains("waking"), "{}", r.message);
    assert!(r.message.contains("until the user comes back"), "{}", r.message);
}

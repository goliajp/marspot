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

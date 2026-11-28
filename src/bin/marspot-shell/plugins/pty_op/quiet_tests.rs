//! When a pane has finished drawing: the input box, the trickle, and a
//! first frame that runs out on a program still drawing.

use super::*;

/// Ticks the runner while the pane keeps printing a spinner: 30
/// bytes a frame, roughly what claude emits while it works.
fn run_spinning(r: &mut OpRunner, host: &Host, state: &FakeState, ms: u64) {
    for _ in 0..(ms / 16) {
        if *host.ended.lock().unwrap() {
            return;
        }
        *state.out_len.lock().unwrap() += 30;
        r.on_tick(host);
        advance(state, 16);
    }
}

const CLAUDE_BOX: &str = "⏺ resumed\n\n✻ Working…\n────────────\n❯\u{a0}\n────────────\n";

/// A resumed claude told to carry on starts working at once, and
/// its spinner never lets the pane go still. Waiting for quiet
/// held the screen frozen for the whole 30 s deadline and then
/// failed the run, skipping what came after it -- on 5 of 9 panes
/// in one profile switch.
#[test]
fn an_input_box_on_screen_ends_the_wait_for_a_pane_that_never_goes_still() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);

    // loading: a spinner, no box yet
    *state.screen.lock().unwrap() = "✳ Resuming conversation…".into();
    run_spinning(&mut r, &host, &state, 3_000);
    assert!(!*host.ended.lock().unwrap(), "no box is a conversation still loading");

    *state.out_len.lock().unwrap() += 40_000;
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    run_spinning(&mut r, &host, &state, 1_000);
    assert!(!*host.ended.lock().unwrap(), "the paint is still inside the window");
    run_spinning(&mut r, &host, &state, 2_000);
    assert!(*host.ended.lock().unwrap(), "the box is up and only the spinner moves");
    assert!(matches!(r.finished, Some(OpOutcome::Done)), "{:?}", r.finished);
}

/// An agent started on a new conversation draws a banner and its
/// input box and stops, well under the floor a first frame of a
/// resumed one is measured by. The box is what says it is ready.
#[test]
fn a_new_conversations_small_first_screen_ends_the_wait_once_its_box_is_up() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.handoff").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);
    *state.out_len.lock().unwrap() += 600;
    run_spinning(&mut r, &host, &state, 3_000);
    assert!(!*host.ended.lock().unwrap(), "600 bytes and no box is startup noise");
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    run_spinning(&mut r, &host, &state, 2_500);
    assert!(matches!(r.finished, Some(OpOutcome::Done)), "{:?}", r.finished);
}

/// A large session is drawn in passes: paint, read more of the
/// transcript, paint again. The box can be up after the first
/// pass; lifting the hold there shows the rest arriving.
#[test]
fn a_box_on_a_screen_that_is_still_repainting_does_not_end_the_wait() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    for _ in 0..6 {
        *state.out_len.lock().unwrap() += 20_000;
        run_spinning(&mut r, &host, &state, 1_000);
        assert!(!*host.ended.lock().unwrap(), "a repaint every second is not settled");
    }
    run_spinning(&mut r, &host, &state, 2_500);
    assert!(*host.ended.lock().unwrap(), "once the passes stop, it is");
}

/// The program the run was started for exits -- a resume it
/// refused. The run ends there rather than at the next deadline.
#[test]
fn a_watched_program_that_exits_ends_the_run_at_once() {
    let (state, env, host) = setup();
    state.present.lock().unwrap().push(77);
    state.alive.lock().unwrap().push(77);
    let op = PtyOp::new("test.cycle")
        .step(Step::watch_process(1, |_| true))
        .step(Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).timeout(Duration::from_secs(30)));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 200);
    assert!(!*host.ended.lock().unwrap(), "running, so the wait goes on");
    state.alive.lock().unwrap().clear();
    run(&mut r, &host, &state, 100);
    assert!(*host.ended.lock().unwrap());
    assert!(
        matches!(&r.finished, Some(OpOutcome::Failed { label: "await_quiet", .. })),
        "{:?}",
        r.finished
    );
}

#[test]
fn watching_a_program_that_never_started_fails_the_run() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle")
        .step(Step::watch_process(1, |_| true))
        .step(Step::send(b"x".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert!(matches!(&r.finished, Some(OpOutcome::Failed { label: "watch_process", .. })));
    assert!(state.sent.lock().unwrap().is_empty(), "nothing after it ran");
}

#[test]
fn an_input_box_does_not_count_below_the_byte_floor() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);
    // the screen still shows the old session's box from before the clear
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    *state.out_len.lock().unwrap() = 300;
    for _ in 0..60 {
        r.on_tick(&host);
        advance(&state, 16);
        // a slow trickle that never reaches the floor
        if *state.out_len.lock().unwrap() < 1_500 {
            *state.out_len.lock().unwrap() += 1;
        }
    }
    assert!(!*host.ended.lock().unwrap());
}

#[test]
fn without_or_composer_a_spinning_pane_still_waits_for_quiet() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake")
        .step(Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048));
    let mut r = OpRunner::new(op, env);
    *state.out_len.lock().unwrap() = 40_000;
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    run_spinning(&mut r, &host, &state, 5_000);
    assert!(!*host.ended.lock().unwrap());
}

#[test]
fn a_sent_message_is_not_an_input_box() {
    // past prompts carry the same glyph, without the rule above
    assert!(!composer_on_screen("❯ fix the bug\n⏺ done\n"));
    assert!(!composer_on_screen("✳ Resuming conversation…"));
    assert!(composer_on_screen(CLAUDE_BOX));
    assert!(composer_on_screen("│ ─────── │\n│ ❯ half a sentence │\n"));
}

/// The two first-frame scripts: a floor, an input box, a deadline, and
/// then the steps that type.
fn first_frame_then_typing(held: &str, late: bool) -> PtyOp {
    let job = Job::new();
    job.finish(Ok((!held.is_empty()).then(|| held.to_string())));
    let wait = Step::await_quiet(Duration::from_millis(1_500))
        .after_bytes(2048)
        .or_composer()
        .timeout(Duration::from_secs(3))
        .named("first_frame");
    PtyOp::new("test.cycle")
        .step(if late { wait.or_drawn_late() } else { wait })
        .step(Step::restore_composer(job))
        .step(Step::send(b"x".to_vec()))
}

/// Ticks while the pane repaints a long history: 400 bytes a frame,
/// far past what a spinner makes.
fn run_repainting(r: &mut OpRunner, host: &Host, state: &FakeState, ms: u64) {
    for _ in 0..(ms / 16) {
        if *host.ended.lock().unwrap() {
            return;
        }
        *state.out_len.lock().unwrap() += 400;
        r.on_tick(host);
        advance(state, 16);
    }
}

fn outcome_line(host: &Host) -> String {
    host.logs.lock().unwrap().iter().find(|l| l.starts_with("test.cycle.outcome")).cloned().unwrap_or_default()
}

/// Pane 482, 2026-11-27: a resumed claude took the terminal and was
/// still repainting a 218 MB conversation when the first frame ran
/// out. It had resumed; the run said it had failed.
#[test]
fn a_first_frame_still_drawing_at_its_deadline_is_drawn_late_and_types_nothing() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(first_frame_then_typing("half a sentence", true), env);
    run_repainting(&mut r, &host, &state, 4_000);
    run(&mut r, &host, &state, 100);
    assert_eq!(
        r.finished,
        Some(OpOutcome::DrawnLate { step: 0, label: "first_frame", skipped: vec!["restore_composer", "send"] })
    );
    assert!(r.finished.as_ref().unwrap().is_done(), "drawn late is not a failure");
    assert!(state.pasted.lock().unwrap().is_empty(), "nothing put back mid-redraw");
    assert!(state.sent.lock().unwrap().is_empty(), "nothing typed mid-redraw");
    let line = outcome_line(&host);
    assert!(line.contains("DrawnLate") && line.contains("closest="), "{line}");
}

/// Steps that do not type still run once the frame came late: a
/// handoff commits its ledger even when the handover is left out.
#[test]
fn a_drawn_late_run_still_runs_the_steps_that_do_not_type() {
    let (state, env, host) = setup();
    let called = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&called);
    let commit: CallFn = Arc::new(move || {
        *flag.lock().unwrap() = true;
        Ok(())
    });
    let op = PtyOp::new("test.cycle")
        .step(
            Step::await_quiet(Duration::from_millis(1_500))
                .after_bytes(2048)
                .or_composer()
                .or_drawn_late()
                .timeout(Duration::from_secs(3))
                .named("first_frame"),
        )
        .step(Step::call(commit).named("commit_ledger"))
        .step(Step::paste("the handover").named("hand_over"));
    let mut r = OpRunner::new(op, env);
    run_repainting(&mut r, &host, &state, 4_000);
    run(&mut r, &host, &state, 100);
    assert!(*called.lock().unwrap(), "the ledger is committed");
    assert!(state.pasted.lock().unwrap().is_empty());
    assert!(
        matches!(&r.finished, Some(OpOutcome::DrawnLate { skipped, .. }) if *skipped == vec!["hand_over"]),
        "{:?}",
        r.finished
    );
}

/// Without the marker the same pane is a failure, as before, and the
/// failure line says what both conditions saw.
#[test]
fn a_first_frame_that_runs_out_says_what_its_two_conditions_saw() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(first_frame_then_typing("", false), env);
    run_repainting(&mut r, &host, &state, 4_000);
    run(&mut r, &host, &state, 100);
    assert_eq!(r.finished, Some(OpOutcome::TimedOut { step: 0, label: "first_frame" }));
    let line = outcome_line(&host);
    for want in ["drew=", "floor=2048B", "quiet=", "grew=", "input_box=no", "closest="] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
}

/// A pane that never reached the floor has not drawn its frame; that
/// stays a failure even with the marker.
#[test]
fn a_first_frame_under_the_floor_at_its_deadline_still_fails() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(first_frame_then_typing("", true), env);
    run(&mut r, &host, &state, 50);
    *state.out_len.lock().unwrap() += 600;
    run(&mut r, &host, &state, 4_000);
    assert_eq!(r.finished, Some(OpOutcome::TimedOut { step: 0, label: "first_frame" }));
    assert!(outcome_line(&host).contains("closest=composer (no input box on screen)"), "{}", outcome_line(&host));
}

/// A pane that paints and goes still is `Done`, and what follows the
/// wait runs.
#[test]
fn a_first_frame_that_goes_quiet_is_done_and_the_run_goes_on() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(first_frame_then_typing("", true), env);
    run(&mut r, &host, &state, 50);
    *state.out_len.lock().unwrap() += 40_000;
    run(&mut r, &host, &state, 2_500);
    assert_eq!(r.finished, Some(OpOutcome::Done));
    assert_eq!(*state.sent.lock().unwrap(), vec![b"x".to_vec()]);
}

#[test]
fn the_nearer_condition_is_named() {
    let base = quiet::Missed {
        drawn: 400_000,
        floor: 2048,
        quiet: Duration::from_millis(100),
        still: Duration::from_millis(1_500),
        grew: Some(40_000),
        boxed: Some(true),
        or_composer: true,
    };
    let text = base.to_string();
    assert!(text.contains("closest=composer (33856B over the trickle allowance)"), "{text}");
    let unboxed = quiet::Missed { drawn: 600, grew: Some(1_000), boxed: Some(false), ..base }.to_string();
    assert!(unboxed.contains("closest=composer (no input box on screen)"), "{unboxed}");
    let short = quiet::Missed { drawn: 600, grew: None, boxed: Some(false), ..base }.to_string();
    assert!(short.contains("closest=quiet (1448B short of the floor)"), "{short}");
    let plain = quiet::Missed { or_composer: false, ..base }.to_string();
    assert!(plain.contains("closest=quiet (1400ms short of still)"), "{plain}");
}

#[path = "quiet_pty_tests.rs"]
mod quiet_pty_tests;

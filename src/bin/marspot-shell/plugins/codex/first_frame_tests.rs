//! A codex resume's first frame on the fake pane: a codex still drawing
//! at the deadline is up, one that never drew its frame is not, and
//! either way the line that ends the wait says what it saw.

use super::pane::{refresh_op, PaneCodex};
use super::update::Release;
use crate::plugins::pty_op::OpOutcome;
use crate::plugins::pty_op::tests::quiet_tests::first_frame_drive::{run_resume, Frame, Ran};
use std::path::Path;

const SHELL: i32 = 4200;
const CODEX: i32 = 4300;

fn refresh(frame: Frame) -> Ran {
    let pane = PaneCodex {
        codex: marspot::pidtree::ProcId { pid: 4242, start_us: 0 },
        shell_pid: SHELL,
        profile: Some(1),
        argv: None,
    };
    let to = Release::parse("0.164.0").unwrap();
    let op = refresh_op(&pane, Path::new("/Users/x/.codex-profile-1"), "01a133f4-8e7b-7331-9630-9c40ff4b467f", None, to)
        .expect("op builds");
    run_resume(op, CODEX, frame)
}

/// A thread at work when its codex is replaced keeps a spinner going
/// once the new TUI has drawn; past the floor that codex is up.
#[test]
fn a_codex_still_drawing_at_the_deadline_is_drawn_late_not_failed() {
    let ran = refresh(Frame::Late);
    let outcome = ran.outcome.clone().expect("the run ended");
    assert!(matches!(outcome, OpOutcome::DrawnLate { label: "first_frame", .. }), "{outcome:?} {:?}", ran.logs);
    assert!(outcome.is_done(), "a switch's end hook does not end the move");
    let line = ran.line("codex.refresh.drawn_late");
    for want in ["drew=", "floor=2048B", "quiet=", "or_composer=false", "closest=quiet"] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
}

/// Startup noise under the floor is no first frame: still a failure,
/// and the outcome line says how far short it was.
#[test]
fn a_codex_under_the_floor_at_the_deadline_times_out_and_says_why() {
    let ran = refresh(Frame::Short);
    assert!(
        matches!(ran.outcome, Some(OpOutcome::TimedOut { label: "first_frame", .. })),
        "{:?} {:?}",
        ran.outcome,
        ran.logs
    );
    let line = ran.line("codex.refresh.outcome");
    for want in ["drew=600B", "floor=2048B", "or_composer=false", "closest=quiet (1448B short of the floor)"] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
}

/// A codex that paints and stops is done, so the two above can fail.
#[test]
fn a_codex_that_paints_and_stops_is_done() {
    let ran = refresh(Frame::Stops);
    assert_eq!(ran.outcome, Some(OpOutcome::Done), "{:?}", ran.logs);
}

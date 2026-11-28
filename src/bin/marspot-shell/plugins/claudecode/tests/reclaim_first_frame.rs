//! The wake's first frame on the fake pane: a claude still repainting
//! at the watchdog has woken, one that never drew its frame has not,
//! and either way the line that ends the wait says what it saw.

use super::*;
use crate::plugins::pty_op::OpOutcome;
use crate::plugins::pty_op::tests::quiet_tests::first_frame_drive::{run_resume, Frame, Ran};

const SHELL: i32 = 4200;
const CLAUDE: i32 = 4300;

fn wake(frame: Frame) -> Ran {
    let op = reclaim_op("aaaa-bbbb", None, None, SHELL, None, &known()).expect("script builds");
    run_resume(op, CLAUDE, frame)
}

/// Pane 482's kind of session, woken instead of switched: claude is up
/// and still repainting a long conversation when the watchdog runs out.
/// That wake worked; dropping the pane's dormant record and its look
/// over a running claude was the wrong answer.
#[test]
fn a_claude_still_repainting_at_the_watchdog_has_woken() {
    let ran = wake(Frame::Late);
    let outcome = ran.outcome.clone().expect("the run ended");
    assert!(matches!(outcome, OpOutcome::DrawnLate { label: "first_frame", .. }), "{outcome:?} {:?}", ran.logs);
    assert!(outcome.is_done(), "the wake end hook keeps the pane");
    let line = ran.line("cc.reclaim.drawn_late");
    for want in ["drew=", "floor=2048B", "quiet=", "grew=", "input_box=", "or_composer=true", "closest="] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
}

/// A resume that only printed startup noise has not drawn its frame:
/// the wake still fails, and its outcome line names both conditions.
#[test]
fn a_wake_under_the_floor_at_the_watchdog_fails_and_says_why() {
    let ran = wake(Frame::Short);
    assert!(
        matches!(ran.outcome, Some(OpOutcome::TimedOut { label: "first_frame", .. })),
        "{:?} {:?}",
        ran.outcome,
        ran.logs
    );
    let line = ran.line("cc.reclaim.outcome");
    for want in ["drew=600B", "floor=2048B", "input_box=no", "or_composer=true", "closest=composer (no input box on screen)"] {
        assert!(line.contains(want), "{want} missing: {line}");
    }
}

/// A claude that paints and stops wakes as before, so the two above
/// can fail.
#[test]
fn a_claude_that_paints_and_stops_wakes_done() {
    let ran = wake(Frame::Stops);
    assert_eq!(ran.outcome, Some(OpOutcome::Done), "{:?}", ran.logs);
    let quiet = ran.logs.iter().any(|l| l.starts_with("cc.reclaim.quiet ") && l.contains("step=first_frame"));
    assert!(quiet, "{:?}", ran.logs);
}

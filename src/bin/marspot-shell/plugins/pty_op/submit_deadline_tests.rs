use super::*;

/// The step's deadline has to outlast the step's own patience.
///
/// It did not: five seconds flat against an eight-second grace, so
/// the wait for the paste could never finish and the sentence was
/// left in the composer. Both numbers were right on their own; the
/// bug was that nothing made them agree.
#[test]
fn the_deadline_outlasts_everything_the_step_waits_for() {
    let worst = SUBMIT_ECHO_GRACE
        + SUBMIT_QUIET
        + SUBMIT_VERIFY_AFTER * SUBMIT_MAX_RETURNS as u32;
    assert!(
        submit_deadline() > worst,
        "a submit is given {:?} to do something that can take {:?}",
        submit_deadline(),
        worst
    );
}

/// And the step that gets built actually carries it.
#[test]
fn a_built_submit_carries_that_deadline() {
    let s = Step::submit("hello");
    assert_eq!(s.timeout, Some(submit_deadline()));
    assert!(
        s.timeout.unwrap() > SUBMIT_ECHO_GRACE,
        "the deadline is shorter than the grace it is supposed to contain"
    );
}

/// Raising the grace has to raise the deadline with it -- that is
/// the whole point of deriving it. Checked by arithmetic on the
/// same expression rather than by editing a constant.
#[test]
fn the_two_cannot_drift_apart() {
    let derived = SUBMIT_ECHO_GRACE
        + SUBMIT_QUIET
        + SUBMIT_VERIFY_AFTER * SUBMIT_MAX_RETURNS as u32
        + Duration::from_secs(2);
    assert_eq!(submit_deadline(), derived, "the deadline stopped being derived");
}

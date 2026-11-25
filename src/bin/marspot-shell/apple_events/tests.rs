use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::child::run_bounded;
use super::{PRESENT_OK, hold_frames_at_most, may_present};

fn gone(pid: u32) -> bool {
    // SAFETY: signal 0 only asks whether the pid exists
    unsafe { libc::kill(pid as i32, 0) != 0 }
}

#[test]
fn a_child_that_does_not_answer_is_killed_at_the_limit() {
    let mut sleeper = Command::new("/bin/sleep");
    sleeper.arg("30");
    let started = Instant::now();
    let (pid, code) = run_bounded(sleeper, Duration::from_millis(300)).expect("sleep starts");
    let took = started.elapsed();
    assert_eq!(code, None, "a killed child has no exit code");
    assert!(took >= Duration::from_millis(300), "returned before the limit: {took:?}");
    assert!(took < Duration::from_secs(3), "waited far past the limit: {took:?}");
    assert!(gone(pid), "the child {pid} is still there: killed and reaped");
}

#[test]
fn a_child_that_answers_gives_its_code() {
    let mut quick = Command::new("/bin/sh");
    quick.args(["-c", "exit 3"]);
    let started = Instant::now();
    let (_, code) = run_bounded(quick, Duration::from_secs(5)).expect("sh starts");
    assert_eq!(code, Some(3));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn frames_are_shown_at_the_deadline_when_nothing_lets_them() {
    PRESENT_OK.store(false, Ordering::Relaxed);
    let from = Instant::now();
    hold_frames_at_most(from, Duration::from_millis(200), None);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!may_present(), "shown before the deadline");
    let by = from + Duration::from_secs(3);
    while !may_present() && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(may_present(), "still held 3 s after a 200 ms deadline");
    assert!(from.elapsed() >= Duration::from_millis(200));
}

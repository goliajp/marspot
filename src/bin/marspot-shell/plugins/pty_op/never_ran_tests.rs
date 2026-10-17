//! A run that never starts still tells whoever submitted it.
//!
//! The plugin that submitted a wake keeps the pane armed until it hears
//! how the run ended; a run dropped before it began used to say nothing.

use super::*;
use std::sync::Mutex;

struct NullIo;
impl PtyIo for NullIo {
    fn send(&self, _: u64, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn hold(&self, _: u64, _: bool) -> std::io::Result<()> {
        Ok(())
    }
    fn paste(&self, _: u64, _: &str) -> std::io::Result<()> {
        Ok(())
    }
    fn reset_mouse_reporting(&self, _: u64) -> std::io::Result<()> {
        Ok(())
    }
}

struct Refusing;
impl OpHost for Refusing {
    fn begin(&self, _: u64, _: Box<dyn PaneSession>) -> Result<(), String> {
        Err("no session for this pane".into())
    }
    fn log(&self, _: LogLevel, _: &str, _: &str) {}
}

fn told() -> (OnEnd, Arc<Mutex<Vec<OpOutcome>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    (Arc::new(move |o: &OpOutcome| s.lock().unwrap().push(o.clone())), seen)
}

fn ops() -> PtyOps {
    PtyOps::new(Arc::new(NullIo) as Arc<dyn PtyIo>)
}

#[test]
fn a_run_refused_by_a_full_queue_is_told_it_failed() {
    let mut ops = ops();
    for _ in 0..MAX_QUEUED_PER_PANE {
        assert!(ops.submit(7, PtyOp::new("filler")).is_some());
    }
    let (f, seen) = told();
    assert!(ops.submit(7, PtyOp::new("late").on_end(f)).is_none());
    assert!(matches!(seen.lock().unwrap().as_slice(), [OpOutcome::Failed { label: "queue", .. }]));
}

#[test]
fn a_run_the_host_will_not_begin_is_told_it_failed() {
    let mut ops = ops();
    let (f, seen) = told();
    ops.submit(7, PtyOp::new("wake").on_end(f));
    ops.pump(&Refusing);
    assert!(matches!(seen.lock().unwrap().as_slice(), [OpOutcome::Failed { label: "begin", .. }]));
}

#[test]
fn a_queued_run_on_a_closed_pane_is_told_it_was_cancelled() {
    let mut ops = ops();
    let (f, seen) = told();
    ops.submit(7, PtyOp::new("first"));
    ops.submit(7, PtyOp::new("wake").on_end(f));
    ops.submit(8, PtyOp::new("other pane"));
    ops.retain(|sid| sid != 7);
    assert!(matches!(seen.lock().unwrap().as_slice(), [OpOutcome::Cancelled("pane closed")]));
}

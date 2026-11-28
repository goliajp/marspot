//! The queue starts its runs through its owner.

use std::sync::Mutex;

use super::super::tests::{FakeIo, FakeState};
use super::*;
use crate::plugins::pty_op::Step;

/// The queue must not need a plugin's permissions to start a run.
///
/// This shipped: `pump` went through `PluginHost::begin_pane_session`,
/// which checks the *current plugin's* permissions — so the very
/// first CLI delivery was refused by the supervisor's own gate with
/// `missing permission`, and the text never reached the pane.  The
/// queue's owner is the authority; starting a run is its business.
#[test]
fn the_queue_starts_runs_through_its_owner_not_a_plugin_gate() {
    struct Owner {
        started: Mutex<Vec<u64>>,
    }
    impl OpHost for Owner {
        fn begin(&self, sid: u64, _s: Box<dyn PaneSession>) -> Result<(), String> {
            self.started.lock().unwrap().push(sid);
            Ok(())
        }
        fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    }
    let state = Arc::new(FakeState::default());
    let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
    let owner = Owner { started: Mutex::new(Vec::new()) };
    ops.submit(390, PtyOp::new("cli.send").step(Step::paste("hi")));
    ops.pump(&owner);
    assert_eq!(*owner.started.lock().unwrap(), vec![390]);
    assert!(ops.is_busy(390));
}

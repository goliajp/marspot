//! The scan worker's thread.

use std::sync::mpsc::{Receiver, Sender};

use super::allocation_adapter::{Allocation, events_dir};
use super::allocation_inputs::{MarksFile, snapshot};
use super::*;

/// Worker thread entry.  Lives until the request channel is dropped
/// (which `stop()` triggers by clearing `scan_req_tx`).
pub(super) fn worker_main(
    mut ctx: WorkerCtx,
    req_rx: Receiver<()>,
    res_tx: Sender<ScanResult>,
) {
    ctx.caches.trends.load();
    let state = marspot::paths::state_root();
    let mut allocation = Allocation::new(events_dir(&state));
    let mut marks = MarksFile::new(state.join("plugins").join("claudecode"));
    while req_rx.recv().is_ok() {
        // Deliberately outside `scan_once`: this one reaches out of
        // marspot and edits Claude Code's settings, and `scan_once`
        // is called directly by tests that have no business doing
        // that to the machine they run on.  The tick owns it.
        for line in ctx.reconcile_statusline() {
            marspot::lx_info!("plugin.claudecode.statusline_hook", &line);
        }
        let result = ctx.scan_once(&crate::observe::shared().latest());
        // recorded and logged only: no pane is moved by it yet
        let now = rooms::unix_now();
        for (tag, line) in allocation.observe(&snapshot(&result, &ctx.seen, marks.read(now), now)) {
            marspot::lx_info!(&*format!("plugin.claudecode.{tag}"), &line);
        }
        if res_tx.send(result).is_err() {
            // Main side hung up; nothing left to do.
            break;
        }
    }
}

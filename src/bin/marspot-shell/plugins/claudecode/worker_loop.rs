//! The scan worker's thread.

use std::sync::mpsc::{Receiver, Sender};

use super::allocation_adapter::{Allocation, events_dir};
use super::allocation_forecast::Forecasting;
use super::allocation_inputs::{MarksFile, snapshot};
use super::allocation_level1::ClassPlan;
use super::allocation_level2::PanePlan;
use super::allocation_more::Hold;
use super::*;

/// Worker thread entry.  Lives until the request channel is dropped
/// (which `stop()` triggers by clearing `scan_req_tx`).
pub(super) fn worker_main(
    mut ctx: WorkerCtx,
    req_rx: Receiver<Option<u64>>,
    res_tx: Sender<ScanResult>,
) {
    ctx.caches.trends.load();
    let state = marspot::paths::state_root();
    let mut allocation = Allocation::new(events_dir(&state));
    let mut forecast = Forecasting::new(&state);
    let mut class_plan = ClassPlan::default();
    let mut pane_plan = PanePlan::new(&state);
    let mut marks = MarksFile::new(state.join("plugins").join("claudecode"));
    while let Ok(focus) = req_rx.recv() {
        // Deliberately outside `scan_once`: this one reaches out of
        // marspot and edits Claude Code's settings, and `scan_once`
        // is called directly by tests that have no business doing
        // that to the machine they run on.  The tick owns it.
        for line in ctx.reconcile_statusline() {
            marspot::lx_info!("plugin.claudecode.statusline_hook", &line);
        }
        let mut result = ctx.scan_once(&crate::observe::shared().latest());
        let now = rooms::unix_now();
        let settings = marspot::settings::get();
        let setting_live = settings.cc_rebalance == marspot::settings::Rebalance::Live;
        allocation.set_live(setting_live, now);
        let snap = snapshot(&result, &ctx.seen, marks.read(now), now);
        let mut lines = allocation.observe(&snap);
        let held = holds(&result, focus);
        lines.extend(allocation.record_more(&snap, &held));
        for (tag, line) in lines {
            marspot::lx_info!(&*format!("plugin.claudecode.{tag}"), &line);
        }
        if let Some(line) = forecast.scan(&allocation.events, &snap.live, now) {
            marspot::lx_info!("sched.forecast", &line);
        }
        if let Some(view) = allocation.view.as_ref()
            && let Some(line) = class_plan.scan(&snap.accounts, view, forecast.last(), allocation.slope_f(), now)
        {
            marspot::lx_info!("sched.level1", &line);
        }
        if let Some(view) = allocation.view.take() {
            pane_plan.focus = focus;
            for (tag, line) in pane_plan.scan(&mut allocation, &snap, &view, forecast.last(), class_plan.last()) {
                marspot::lx_info!(tag, &line);
            }
            allocation.view = Some(view);
        }
        let reset = settings.sched.overrides.iter().find(|(k, _)| k == "gate_reset").map(|(_, v)| *v as i64);
        let (orders, live, exec_lines) = pane_plan.execute(&mut allocation, &snap, &held, setting_live, reset);
        for (tag, line) in exec_lines {
            marspot::lx_info!(tag, &line);
        }
        (result.plan_orders, result.plan_live) = (orders, live);
        result.alloc = allocation.view.take();
        if res_tx.send(result).is_err() {
            // Main side hung up; nothing left to do.
            break;
        }
    }
}

/// What each bound pane has that would hold a move, besides its transcript.
pub(super) fn holds(result: &ScanResult, focus: Option<u64>) -> HashMap<u64, Hold> {
    let hold = |sid: u64, m: &BindMeta| {
        let vetoes = result.new_vetoes.get(&sid).copied();
        Hold { child: vetoes.map(|v| v.0), timer: vetoes.map(|v| v.1), helpers: result.helpers.sits_out(sid), focus: focus == Some(sid), claude_pid: m.claude.pid }
    };
    result.new_meta.iter().map(|(sid, m)| (*sid, hold(*sid, m))).collect()
}

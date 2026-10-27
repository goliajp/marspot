//! Moving a pane whose codex is older than the installed one onto it.

use super::pane::refresh_op;
use super::rollout::codex_home;
use super::{update, CodexPlugin};
use crate::plugins::peerbus::ThreadView;
use crate::plugins::{pidtree, LogLevel, PluginHost};

impl CodexPlugin {
    /// Restart, at most one per tick and one at a time, a pane whose
    /// codex is older than the installed one (see `update`).
    pub(super) fn refresh_stale(&mut self, host: &dyn PluginHost, panes: &[(u64, pidtree::ProcId, Option<Vec<String>>)]) {
        let live = panes.iter().map(|(sid, codex, _)| (*sid, *codex)).collect();
        self.refresher.prune(&live);
        if panes.is_empty() || !marspot::settings::get().codex_follow_updates {
            return;
        }
        if self.refresher.busy.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let Some(home) = codex_home() else { return };
        let Some(installed) = update::installed_cli(&home) else { return };
        let mut threads = crate::plugins::peerbus::threads();
        threads.extend(dev_threads(&home, panes));
        for (sid, codex, argv) in panes {
            let Some(running) = self.refresher.release_of(*codex) else { continue };
            if running >= installed {
                self.refresher.held.remove(sid);
                continue;
            }
            let view = threads.get(sid).filter(|v| v.codex == *codex);
            // a pane's terminal is quiet when its status has settled back
            // from `Output` to the plain foreground job, and for how long
            let quiet_for = match host.pane_status(*sid) {
                Ok(Some(v)) if v.status == marspot::pane_state::PaneStatus::Busy(marspot::pane_state::BusyKind::ForegroundJob) => v.held,
                _ => std::time::Duration::ZERO,
            };
            let evidence = update::Evidence {
                tried: self.refresher.tried.get(sid) == Some(&installed),
                thread_known: view.is_some(),
                plain_argv: argv.as_deref().is_some_and(update::plain_argv),
                user_here: self.focused.map(|f| f == *sid),
                thread_idle: view.is_some_and(|v| v.idle),
                quiet_for,
            };
            if let Some(why) = update::hold_reason(&evidence) {
                if self.refresher.held.insert(*sid, why) != Some(why) {
                    host.log(LogLevel::Info, "refresh.held", &format!("pane {sid}: codex {running} < {installed}, held: {why}"));
                }
                continue;
            }
            let (Some(view), Some(pane)) = (view, self.panes.get(sid)) else { continue };
            self.refresher.tried.insert(*sid, installed);
            let cwd = pidtree::proc_cwd(codex.pid).map(|c| c.to_string_lossy().into_owned());
            let Some(op) = refresh_op(pane, &view.home, &view.thread, cwd, installed) else {
                host.log(LogLevel::Warn, "refresh.no_command", &format!("pane {sid}: cannot build the resume line"));
                continue;
            };
            let busy = std::sync::Arc::clone(&self.refresher.busy);
            busy.store(true, std::sync::atomic::Ordering::Release);
            let op = op.on_end(std::sync::Arc::new(move |_| busy.store(false, std::sync::atomic::Ordering::Release)));
            host.log(LogLevel::Info, "refresh.start", &format!("pane {sid}: codex {running} → {installed}, thread {}", view.thread));
            if let Err(e) = host.submit_pty_op(*sid, op) {
                self.refresher.busy.store(false, std::sync::atomic::Ordering::Release);
                host.log(LogLevel::Warn, "refresh.submit_failed", &format!("pane {sid}: {e}"));
            }
            return;
        }
    }
}

/// Dev seam (`MARSPOT_DEV_CODEX_THREADS=<path>`): a file a test writes
/// `<sid> <thread>` lines into, each read as an idle thread of that
/// pane's codex.  A test's stand-in codex has no daemon for peerbus to
/// bind it against.  Unset in the installed app.
fn dev_threads(home: &std::path::Path, panes: &[(u64, pidtree::ProcId, Option<Vec<String>>)]) -> Vec<(u64, ThreadView)> {
    let Some(path) = std::env::var_os("MARSPOT_DEV_CODEX_THREADS") else { return Vec::new() };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (sid, thread) = l.trim().split_once(' ')?;
            let sid: u64 = sid.parse().ok()?;
            let (_, codex, _) = panes.iter().find(|(s, ..)| *s == sid)?;
            Some((sid, ThreadView { codex: *codex, home: home.to_path_buf(), thread: thread.trim().to_string(), idle: true }))
        })
        .collect()
}

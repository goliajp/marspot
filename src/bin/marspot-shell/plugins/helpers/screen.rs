//! Judging a helper from its still screen: for the agents no plugin
//! reports turns for (codex, gemini), a thread per helper asleep on a
//! quiet wait in its pane.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, ErrorCode};

use super::Shared;
use super::notify::Stop;
use super::work::Job;
use crate::pane_control::helpers_port::{Waited, with_port};

/// How long a helper's screen must be still to be judged.
fn quiet_ms(agent: AgentKind) -> u32 {
    match agent {
        AgentKind::Codex | AgentKind::Claude => 15_000,
        // not seen to redraw while a tool runs: a slow build must not read as a stop
        AgentKind::Gemini | AgentKind::Kimi => 60_000,
    }
}

/// What a still screen says about a helper; None when it is not judged.
fn judge(sid: u64) -> Option<Stop> {
    let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
    let s = obs.session(sid)?;
    if s.shell.is_some() && s.front_pgid == 0 {
        return Some(Stop::Exited);
    }
    match marspot::pane_read::composer_box(sid) {
        Ok(Some(t)) if t.trim().is_empty() => Some(Stop::Idle),
        // something is typed in its box: not a stop
        Ok(Some(_)) => None,
        _ => Some(Stop::Blocked),
    }
}

/// Sleep on quiet waits in `sid` until it ends; each time it falls
/// quiet, judge the screen.  The next wait counts from where this one
/// came true, so it needs new output first.  It ends with the start of
/// the plugin it belongs to (`epoch`): the next start has its own.
pub(super) fn watch_screen(s: &Arc<Shared>, sid: u64, agent: AgentKind, mut since: Option<u64>, epoch: u64) {
    let current = || s.epoch.load(Ordering::Acquire) == epoch;
    while current() && s.table().map.contains_key(&sid) {
        let waited = with_port(None, |p| p.wait_quiet(sid, quiet_ms(agent), since, Instant::now() + Duration::from_secs(1800)));
        if !current() {
            return;
        }
        match waited {
            Some(Ok(Waited::Met { pos })) => {
                since = Some(pos);
                if let Some(stop) = judge(sid) {
                    s.queue_wait(Job::Stopped { sid, stop, by_screen: true });
                    if stop == Stop::Exited {
                        return;
                    }
                }
            }
            Some(Ok(Waited::Timeout)) => {}
            // the shell started before its core, or the core is being replaced
            Some(Err(r)) if r.code == ErrorCode::CoreRestarting => std::thread::sleep(Duration::from_millis(r.retry_after_ms.unwrap_or(1_000))),
            Some(Err(r)) => {
                marspot::lx_warn!("helpers.watch_failed", &format!("sid={sid}: {}: {}", r.code.as_str(), r.message));
                return;
            }
            Some(Ok(Waited::Ended)) | None => return,
        }
    }
}

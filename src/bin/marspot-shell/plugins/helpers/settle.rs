//! The first moments of a new helper, on a thread of its own: hand
//! kimi its task, see the agent past its first screens, then watch the
//! screen of the agents no plugin reports turns for.

use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, MAX_SEND_BYTES};

use super::Shared;
use super::first_screens::First;
use super::notify::Stop;
use super::table::Event;
use super::screen::watch_screen;
use super::work::Job;
use crate::pane_control::helpers_port::{Waited, with_port};

/// After the start line went in at `pos`: hand kimi its task, get the
/// agent past its first screens, then watch the screen of the agents no
/// plugin reports turns for.
pub(super) fn settle(s: Arc<Shared>, sid: u64, agent: AgentKind, pos: u64) {
    let f = First { epoch: s.epoch(), s, sid, agent };
    spawn(sid, move || {
        if agent == AgentKind::Kimi && !deliver_to_kimi(&f, pos) {
            f.s.queue_wait(Job::Stopped { sid, stop: Stop::Exited, by_screen: true });
            return;
        }
        follow(&f, pos);
    });
}

/// A helper carried over from before the shell started: it may still be
/// at its first screens.
pub(super) fn resume(s: &Arc<Shared>, sid: u64, agent: AgentKind) {
    let f = First { epoch: s.epoch(), s: Arc::clone(s), sid, agent };
    spawn(sid, move || follow(&f, 0));
}

fn spawn(sid: u64, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name("helpers-settle".into()).spawn(f) {
        marspot::lx_warn!("helpers.settle_spawn_failed", &format!("sid={sid}: {e}"));
    }
}

/// From `pos`, where the agent started: it may have printed all it will
/// already.
fn follow(f: &First, pos: u64) {
    f.pass(pos);
    if f.s.contained(f.sid) || matches!(f.agent, AgentKind::Codex | AgentKind::Gemini) {
        watch_screen(&f.s, f.sid, f.agent, Some(pos), f.epoch);
    }
}

/// Once kimi's screen is up, its task: the task file, header and all, as
/// it says where the result goes.
fn deliver_to_kimi(f: &First, pos: u64) -> bool {
    let until = Instant::now() + Duration::from_secs(60);
    let quiet = |since| with_port(None, |p| p.wait_quiet(f.sid, 1_500, Some(since), until));
    let mut first = quiet(pos);
    // a container takes seconds to start, which is as quiet as a screen that is up
    while let Some(Ok(Waited::Met { pos: at })) = &first
        && f.s.contained(f.sid)
        && !f.has_drawn()
    {
        first = quiet(*at);
    }
    let up = matches!(first, Some(Ok(Waited::Met { .. })));
    let task = std::fs::read_to_string(f.s.dir_of(f.sid).join("task.md")).ok();
    let delivered = up && task.as_deref().is_some_and(|t| paste_task(f.sid, t));
    if delivered {
        f.s.record(f.sid, Event::TaskDelivered);
    }
    delivered
}

/// kimi's task, pasted one send's worth at a time, Enter after the last.
fn paste_task(sid: u64, task: &str) -> bool {
    let mut rest = task.trim_end();
    while !rest.is_empty() {
        let mut cut = rest.len().min(MAX_SEND_BYTES);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (part, after) = rest.split_at(cut);
        if !with_port(None, |p| p.send_text(sid, part, after.is_empty())).is_some_and(|r| r.is_ok()) {
            return false;
        }
        rest = after;
    }
    true
}

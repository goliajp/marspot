//! A container helper whose image is not built yet: built on a thread
//! of its own, after which its start goes on.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, ErrorCode};

use super::super::table::Event;
use super::super::work::Job;
use super::super::{Shared, launch, settle};
use super::{docker, image, login};
use crate::pane_control::helpers_port::with_port;

const PROMPT_WITHIN: Duration = Duration::from_secs(10);
/// How long a core or a socket that is starting is waited for.
const CORE_WITHIN: Duration = Duration::from_secs(60);
const NOT_BUILT: &str = "was closed: the container image it needs could not be built within 15 minutes; marspot's log has the reason.";
const NOT_LASTING: &str = "was closed: while its container image was built, the account's login came to last less than its budget and 10 minutes; it is renewed when the agent runs on that account on the host.";
const NOT_TYPED: &str = "was closed: its container image is there, but the line that starts it did not go in.";

/// Build the image, then hand helper `sid` its task.
pub(in super::super) fn prepare(s: &Arc<Shared>, sid: u64, agent: AgentKind) {
    let (me, epoch) = (Arc::clone(s), s.epoch());
    let spawned = std::thread::Builder::new().name("helpers-image".into()).spawn(move || {
        let built = docker::up(Instant::now() + docker::ANSWER).map_err(|r| r.message).and_then(|d| image::ready(&d));
        // put away or started again: the next start builds anew
        if !me.runs(epoch) || !me.table().map.contains_key(&sid) {
            return;
        }
        let done = built.map_err(|e| (NOT_BUILT, e)).and_then(|()| deliver(&me, sid, agent));
        if let Err((tell, why)) = done {
            marspot::lx_warn!("helpers.prepare_failed", &format!("sid={sid}: {why}"));
            if me.lock().closing.insert(sid) {
                me.queue_wait(Job::Close { sid, tell: Some(tell) });
            }
        }
    });
    if let Err(e) = spawned {
        marspot::lx_warn!("helpers.prepare_spawn_failed", &format!("sid={sid}: {e}"));
        s.queue_closing(sid, Job::Close { sid, tell: Some(NOT_BUILT) });
    }
}

fn deliver(s: &Arc<Shared>, sid: u64, agent: AgentKind) -> Result<(), (&'static str, String)> {
    let failed = |e: String| (NOT_TYPED, e);
    let dir = s.dir_of(sid);
    image::name_in(&dir).map_err(|e| failed(e.to_string()))?;
    let Some(h) = s.table().map.get(&sid).cloned() else { return Ok(()) };
    let handed = matches!(agent, AgentKind::Claude | AgentKind::Codex);
    if handed
        && let Some(l) = login::of_label(agent, &h.account)
        && l.expires_at() > login::expires_ms(&dir)
    {
        login::hand_over(&dir, &l).map_err(|r| failed(r.message))?;
    }
    if handed && !login::still_lasts(&dir, h.started.budget_min) {
        return Err((NOT_LASTING, "the login lasts too short a time by now".into()));
    }
    let pos = type_line(s, sid, &launch::start_line(&dir)).map_err(failed)?;
    if agent != AgentKind::Kimi {
        s.record(sid, Event::TaskDelivered);
    }
    settle::settle(Arc::clone(s), sid, agent, pos);
    Ok(())
}

/// Type `line` at the shell of helper `sid`; where it went in.
pub(super) fn type_line(s: &Shared, sid: u64, line: &str) -> Result<u64, String> {
    let (epoch, until) = (s.epoch(), Instant::now() + CORE_WITHIN);
    loop {
        let wait = match with_port(None, |p| p.type_at_prompt(sid, line, PROMPT_WITHIN)) {
            Some(Ok(pos)) => return Ok(pos),
            // the shell started before its core, or the core is being replaced
            Some(Err(r)) if r.code == ErrorCode::CoreRestarting => r.retry_after_ms.unwrap_or(1_000),
            // a container was just removed, and its script is ending
            Some(Err(r)) if r.code == ErrorCode::NotAtShell => 250,
            Some(Err(r)) => return Err(format!("{}: {}", r.code.as_str(), r.message)),
            None => 1_000,
        };
        if Instant::now() >= until || !s.runs(epoch) {
            return Err("the pane could not be reached".into());
        }
        std::thread::sleep(Duration::from_millis(wait));
    }
}

/// The line that starts the container of the helper at `dir` again.
pub(super) fn resume_line(dir: &Path) -> String {
    format!("sh '{}'", dir.join("resume.sh").display())
}

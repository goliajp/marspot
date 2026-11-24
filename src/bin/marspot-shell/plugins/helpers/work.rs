//! The helpers' threads: one worker that tells parents and closes
//! helpers, in order; one per helper until it is past its first
//! screens (`first_screens`); and, for the agents no plugin reports turns for
//! (codex, gemini), one per helper asleep on a quiet wait in its pane.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::Duration;

use marspot_peer::control::AgentKind;

use super::close::close;
use super::notify::{self, Stop, Verdict};
use super::pair::Moved;
use super::pending::{Notice, front_of};
use super::table::{Event, Helper};
use super::{Shared, records};
use crate::observe::Observation;
use crate::pane_control::names::full_name_or_entry as full_name;
use crate::plugins::claudecode::with_helpers::{NOTICE_HEAD, drain_account_changes};
use crate::plugins::usage::for_helpers;

const JOBS: usize = 64;

pub(super) enum Job {
    /// Something came due sooner: work out the sleep again.
    Wake,
    Stopped { sid: u64, stop: Stop, by_screen: bool },
    /// At an approval prompt, told of one before the shell started.
    StillBlocked { sid: u64 },
    /// The pane of helpers' parent was handed to another agent.
    ParentMoved(Moved),
    /// Tell the stops the rate held back.
    Release { sid: u64 },
    /// Close a helper; `tell`: say why to its parent.
    Close { sid: u64, tell: Option<&'static str> },
    /// Its session was missing from the last walk: close it if a fresh
    /// one agrees.
    Gone { sid: u64 },
    /// Look at how large the codex helpers' contexts are.
    Contexts,
    /// Write down what the main loop saw at `at_ms`.
    Fact { sid: u64, at_ms: u64, ev: Event },
    /// Try an undelivered notice again.
    Redeliver { sid: u64 },
}

impl Job {
    /// The helper it is about; 0 for none.
    pub fn sid(&self) -> u64 {
        match self {
            Job::Wake | Job::Contexts => 0,
            Job::ParentMoved(m) => m.pane,
            Job::Stopped { sid, .. } | Job::StillBlocked { sid } | Job::Release { sid } | Job::Close { sid, .. } | Job::Gone { sid } | Job::Fact { sid, .. } | Job::Redeliver { sid } => *sid,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Job::Wake => "wake",
            Job::Stopped { .. } => "stopped",
            Job::StillBlocked { .. } => "still_blocked",
            Job::ParentMoved(_) => "parent_moved",
            Job::Release { .. } => "release",
            Job::Close { .. } => "close",
            Job::Gone { .. } => "gone",
            Job::Contexts => "contexts",
            Job::Fact { .. } => "fact",
            Job::Redeliver { .. } => "redeliver",
        }
    }
}

/// Start a worker for this start of the plugin, and queue to it from now
/// on; one from an earlier start ends after the job it is at.
pub(super) fn start(s: Arc<Shared>) -> std::io::Result<()> {
    let (tx, rx) = sync_channel(JOBS);
    let epoch = s.arm(tx);
    let w = Arc::clone(&s);
    let spawned = std::thread::Builder::new().name("helpers".into()).spawn(move || run(&w, rx, epoch));
    if spawned.is_err() {
        s.park();
    }
    spawned.map(drop)
}

/// Jobs as they come, and what the clock brings, in one order.
fn run(s: &Arc<Shared>, rx: Receiver<Job>, epoch: u64) {
    loop {
        let job = match super::timers::sleep_for(s) {
            Some(d) => match rx.recv_timeout(d) {
                Ok(j) => Some(j),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(j) => Some(j),
                Err(_) => return,
            },
        };
        if ended(s, epoch) {
            return;
        }
        if let Some(job) = job {
            handle(s, job);
        }
        // a job can take a minute: a start since then has a worker of its own
        if ended(s, epoch) {
            return;
        }
        account_changes(s);
        for job in super::timers::due_jobs(s, epoch, records::now_ms(), super::timers::no_agent_now) {
            if ended(s, epoch) {
                return;
            }
            handle(s, job);
        }
    }
}

/// Whether the start this worker belongs to is over.
fn ended(s: &Shared, epoch: u64) -> bool {
    let over = s.epoch.load(Ordering::Acquire) != epoch;
    if over {
        marspot::lx_info!("helpers.parked", "the app was put away or started again; this helpers worker ends");
    }
    over
}

/// A helper moved to another account after a refusal: that is where it
/// runs now.
pub(super) fn account_changes(s: &Shared) {
    for c in drain_account_changes() {
        if s.table().map.contains_key(&c.sid) {
            s.record(c.sid, Event::AccountChanged(format!("P{}", c.to)));
        }
    }
}

fn handle(s: &Arc<Shared>, job: Job) {
    match job {
        Job::Wake => s.wake_queued.store(false, Ordering::Release),
        Job::Stopped { sid, stop, by_screen } => {
            marspot::lx_info!("helpers.job", &format!("sid={sid} stopped {stop:?} by_screen={by_screen}"));
            stopped(s, sid, stop, by_screen);
        }
        Job::StillBlocked { sid } => still_blocked(s, sid, screen_end(sid, notify::SCREEN_LINES)),
        Job::ParentMoved(m) => {
            let obs = crate::observe::shared().fresh(Duration::ZERO);
            let front = obs.session(m.pane).and_then(|x| x.front()).map(|p| p.row.id());
            super::pair::parent_moved(s, &m, front);
        }
        Job::Release { sid } => {
            if let Some(h) = s.table().map.get(&sid).cloned().filter(|h| h.suppressed > 0 && h.may_notify(records::now_ms())) {
                marspot::lx_info!("helpers.job", &format!("sid={sid} release"));
                // a held stop at a prompt is still one
                tell(s, &h, if h.held_blocked { Stop::Blocked } else { Stop::Idle });
            }
        }
        Job::Close { sid, tell } => {
            marspot::lx_info!("helpers.job", &format!("sid={sid} close told={}", tell.is_some()));
            let h = s.table().map.get(&sid).cloned();
            let name = full_name(sid, &crate::observe::shared().fresh(crate::observe::INTERVAL));
            // closed first: it waits for a core that is still starting
            if close(s, sid)
                && let (Some(why), Some(h)) = (tell, h)
            {
                let text = format!("{NOTICE_HEAD} Helper \"{}\" {why} {}", notify::shown(&name), marspot_peer::notice::TRAILER);
                send(s, sid, Notice { parent: h.started.parent, body: text, ..Notice::default() });
            }
        }
        Job::Gone { sid } => {
            if crate::observe::shared().fresh(crate::observe::INTERVAL).session(sid).is_none() {
                close(s, sid);
            } else {
                s.lock().closing.remove(&sid);
            }
        }
        Job::Contexts => s.look_at_contexts(&crate::plugins::peerbus::threads()),
        Job::Fact { sid, at_ms, ev } => {
            if s.table().map.contains_key(&sid) {
                s.record_seen(sid, at_ms, ev);
            }
        }
        Job::Redeliver { sid } => {
            let n = s.lock().undelivered.take(sid);
            if let Some(n) = n {
                marspot::lx_info!("helpers.job", &format!("sid={sid} redeliver try={}", n.tries + 1));
                send(s, sid, n);
            }
        }
    }
}

/// Send notice `n` about helper `sid` to its parent as named now.  The
/// facts it stands for are written once it went; one that did not go is
/// kept for the parent's next program.  Nothing here waits: a parent
/// whose agent is not named or has no inbox yet (the shell names its
/// panes a moment after it starts; an agent registers a moment after it
/// starts) is tried again on the worker's clock (`pending`).
fn send(s: &Shared, sid: u64, mut n: Notice) {
    s.lock().undelivered.superseded(sid);
    let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
    let parent = full_name(n.parent, &obs);
    n.tries += 1;
    match notify::deliver(&parent, &n.body) {
        Ok(()) => {
            marspot::lx_info!("helpers.notice", &format!("sid={sid}: delivered"));
            super::pair::settle_now(s, n.parent);
            if s.table().map.contains_key(&sid) {
                for ev in n.events {
                    s.record(sid, ev);
                }
            }
        }
        Err(e) => not_delivered(s, sid, n, &e, &obs, records::now_ms()),
    }
}

/// Keep `n`, refused with `e` at `now_ms` by its parent as `obs` shows
/// it.  What the refusal means and the program a later one is compared
/// with come from the one look: a program that came since is a change.
pub(super) fn not_delivered(s: &Shared, sid: u64, mut n: Notice, e: &str, obs: &Observation, now_ms: u64) {
    let front = front_of(obs, n.parent);
    let registering = front.is_some_and(|pgid| pgid != 0) && super::pending::not_ready(e);
    n.failed(registering, front, now_ms);
    match n.retry_ms {
        Some(at) => marspot::lx_info!("helpers.notice", &format!("sid={sid}: not delivered, again at {at}: {e}")),
        None => marspot::lx_warn!("helpers.notice", &format!("sid={sid}: not delivered (try {}): {e}", n.tries)),
    }
    let kept = s.lock().undelivered.keep(sid, n);
    s.strip_changed();
    if let Some(h) = crate::pane_control::hub() {
        h.knock();
    }
    // the worker of a later start may be the one to try it
    s.wake();
    if !kept {
        marspot::lx_warn!("helpers.notice_dropped", &format!("sid={sid}: not delivered in {} tries", super::pending::TRIES));
    }
}

fn stopped(s: &Arc<Shared>, sid: u64, stop: Stop, by_screen: bool) {
    let Some(h) = s.table().map.get(&sid).cloned() else { return };
    match notify::verdict(&h, stop, by_screen, records::now_ms()) {
        Verdict::Skip => return,
        Verdict::Hold => return s.record(sid, Event::NoticeSuppressed { blocked: stop == Stop::Blocked }),
        Verdict::Send => tell(s, &h, stop),
    }
    if stop.is_last() {
        close(s, sid);
    }
}

/// The bottom of helper `sid`'s screen; None when it cannot be read.
fn screen_end(sid: u64, lines: usize) -> Option<String> {
    marspot::pane_read::screen(sid, 0).ok().map(|t| notify::screen_end(&t, lines))
}

/// Helper `sid`, told of at a prompt before the shell started, is at one
/// with `screen`: told when that is known to be another.
pub(super) fn still_blocked(s: &Arc<Shared>, sid: u64, screen: Option<String>) {
    let Some(h) = s.table().map.get(&sid).cloned() else { return };
    let now = screen.map(|t| notify::prompt_sha(&t));
    let same = h.blocked_prompt.is_empty() || now.is_none_or(|n| n == h.blocked_prompt);
    marspot::lx_info!("helpers.job", &format!("sid={sid} still_blocked same={same}"));
    if !same {
        stopped(s, sid, Stop::Blocked, false);
    }
}

/// What a claude helper's stop says when every account refuses its
/// model; None while one can take it.
fn refused_note(h: &Helper, stop: Stop) -> Option<String> {
    if h.started.agent != AgentKind::Claude || h.contained() || stop.is_last() {
        return None;
    }
    let model = Some(h.started.model.as_str()).filter(|m| *m != "default");
    let r = for_helpers::claude_all_refused(model)?;
    let earliest = for_helpers::earliest_text(&r).map_or_else(|| "at a time the usage feed does not give".to_string(), |t| format!("at {t}"));
    Some(format!("All claude accounts are refusing it (out of quota); the earliest recovers {earliest}. Per account: {}.", for_helpers::recoveries_text(&r)))
}

/// Send the notice for a stop of `h`, and write down that it went.
fn tell(s: &Arc<Shared>, h: &Helper, stop: Stop) {
    let sid = h.sid;
    let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
    let path = s.result_path(sid);
    let result = notify::read_result(&path);
    // claude's own stop is the plugin's report, not a guess from the screen
    let lines = match (stop, h.started.agent) {
        (Stop::Budget, _) => 2 * notify::SCREEN_LINES,
        (Stop::Idle, AgentKind::Claude) if !h.contained() => 0,
        _ => notify::SCREEN_LINES,
    };
    let screen = (lines > 0).then(|| screen_end(sid, lines)).flatten();
    let over = super::container::at_stop(s, h, stop, screen.as_deref());
    let note = over.as_ref().map(|o| o.note.clone()).or_else(|| refused_note(h, stop));
    let body = notify::text(&full_name(sid, &obs), h, stop, &path, result.as_deref(), screen.as_deref(), note.as_deref());
    let mut events = vec![match stop {
        Stop::Idle => Event::IdleNoticed,
        Stop::Blocked => Event::BlockedNoticed { prompt: screen.as_deref().map_or_else(String::new, notify::prompt_sha) },
        Stop::Exited => Event::ExitedNoticed,
        Stop::Budget => Event::BudgetExpired,
    }];
    if let Some(r) = result.as_deref() {
        events.push(Event::ResultDelivered { sha: notify::sha(r) });
    }
    send(s, sid, Notice { parent: h.started.parent, body, events, ..Notice::default() });
    if over.is_some_and(|o| o.renewed) {
        super::container::restart(s, sid, h.account.clone());
    }
}

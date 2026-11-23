//! A new helper until its agent is past its first screens: the dialogs
//! it puts up are answered, and a parent whose helper stays at one of
//! them is told.  Everything here sleeps on quiet waits in the pane.

use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::Shared;
use super::dialogs::{Ask, Dialog, Look, Press, look};
use super::launch;
use super::notify::Stop;
use super::passing::{Keys, drawn, give_enter_back, nothing_sent, take_enter, take_told};
use super::work::Job;
use crate::pane_control::helpers_port::{Waited, with_port};

/// A screen still this long is read.
const DRAWN_MS: u32 = 2_000;
/// A screen still this long with no input box is a stop.
const STOPPED_MS: u32 = 10_000;
/// How long a dialog may take to act on a key.
const KEY_TAKEN: Duration = Duration::from_secs(10);

/// What became of a dialog's keys.
#[derive(Debug, PartialEq)]
pub(super) enum Pressed {
    /// The Enter went in here, to this screen, and the dialog is gone.
    Entered(u64, String),
    /// A key did not go, and why.
    NotSent(&'static str),
    /// It gets no key any more.
    Over,
}

pub(super) struct First {
    pub s: Arc<Shared>,
    pub sid: u64,
    pub agent: AgentKind,
    /// The plugin start it belongs to.
    pub epoch: u64,
}

impl First {
    fn current(&self) -> bool {
        self.s.epoch() == self.epoch && self.s.table().map.contains_key(&self.sid)
    }

    /// Whether the agent's plugin reports its turns, so that nothing
    /// else judges its screen.
    fn by_turns(&self) -> bool {
        !self.s.contained(self.sid) && matches!(self.agent, AgentKind::Claude | AgentKind::Kimi)
    }

    fn look(&self) -> (String, Look) {
        let screen = marspot::pane_read::screen(self.sid, 0).unwrap_or_default();
        let input = marspot::pane_read::composer_box(self.sid).ok().flatten();
        let seen = look(self.agent, &screen, input.as_deref());
        (screen, seen)
    }

    fn at_shell(&self) -> bool {
        let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
        obs.session(self.sid).is_some_and(|x| x.shell.is_some() && x.front_pgid == 0)
    }

    /// Why `d` is left alone: saying yes would trust more than the
    /// directory the caller asked for (for gemini, the helper's own that
    /// it is given besides), or take a mode it did not ask for.
    pub(super) fn refusal(&self, d: &Dialog) -> Option<&'static str> {
        if d.ask == Ask::Bypass {
            let asked = self.s.table().map.get(&self.sid).is_some_and(|h| launch::asks_bypass(&h.started.mode));
            return (!asked).then_some("left, bypass was not asked for");
        }
        let own = self.s.dir_of(self.sid);
        let ours = if self.agent == AgentKind::Gemini {
            d.is_about(&self.s.own_dir_shown(self.sid))
        } else {
            let task = std::fs::read_to_string(own.join("task.md")).unwrap_or_default();
            task_dir(&task).is_some_and(|dir| d.is_about(dir.as_ref()))
        };
        (!ours).then_some("left, not the helper's directory")
    }

    fn note(&self, d: &Dialog, result: &str) {
        let ask = d.ask.word();
        let (agent, dirs) = (self.agent.as_str(), d.dirs_text());
        let result = if dirs.is_empty() { result.to_string() } else { format!("{result}: {dirs}") };
        // the audit log keeps the start of a detail only, and all of a result
        with_port(None, |p| p.audit("helper.trust", self.sid, &format!("{agent} {ask}"), &result));
        marspot::lx_info!("helpers.trust", &format!("sid={} {agent} {ask} {result}", self.sid));
    }

    /// Wait for the pane to print past `since` and be still; None once
    /// there is nothing to wait for.
    fn still(&self, quiet_ms: u32, since: u64) -> Option<u64> {
        while self.current() {
            match with_port(None, |p| p.wait_quiet(self.sid, quiet_ms, Some(since), Instant::now() + Duration::from_secs(1800))) {
                Some(Ok(Waited::Met { pos })) => return self.current().then_some(pos),
                Some(Ok(Waited::Timeout)) => {}
                // the shell started before its core, or the core is being replaced
                Some(Err(r)) if r.code == ErrorCode::CoreRestarting => std::thread::sleep(Duration::from_millis(r.retry_after_ms.unwrap_or(1_000))),
                Some(Err(r)) => {
                    marspot::lx_warn!("helpers.first_wait_failed", &format!("sid={}: {}: {}", self.sid, r.code.as_str(), r.message));
                    return None;
                }
                Some(Ok(Waited::Ended)) | None => return None,
            }
        }
        None
    }

    /// Until the agent shows its input box, takes a turn or leaves the
    /// pane; `since`: where in its output the agent started.
    pub fn pass(&self, mut since: u64) {
        let (mut quiet, mut before, mut left) = (DRAWN_MS, String::new(), None::<Dialog>);
        let mut keys = [Keys::default(), Keys::default()];
        while let Some(pos) = self.still(quiet, since) {
            let (screen, seen) = self.look();
            if seen == Look::Input || self.by_turns() && self.s.lock().turned.contains(&self.sid) {
                return;
            }
            if let Look::Ask(d) = &seen {
                if let Some(why) = self.refusal(d) {
                    if left.as_ref() != Some(d) {
                        self.note(d, why);
                        left = Some(d.clone());
                    }
                } else if let Some((at, shown)) = self.answer(d, &mut keys[d.ask as usize]) {
                    (since, quiet, before) = (at, DRAWN_MS, shown);
                    continue;
                }
            }
            if quiet < STOPPED_MS {
                quiet = STOPPED_MS;
                continue;
            }
            if self.at_shell() {
                return;
            }
            if let Some(job) = self.stop_at(&screen, &seen, &before) {
                self.s.queue_wait(job);
            }
            // without a plugin, a screen that is no dialog is the watch's
            // to judge, once there is one
            if !self.by_turns() && self.is_drawn(&screen, &seen, &before) {
                return;
            }
            (since, quiet) = (pos, DRAWN_MS);
        }
    }

    /// Whether anything is under the line that started it.
    pub(super) fn has_drawn(&self) -> bool {
        drawn(&self.look().0, &launch::start_line(&self.s.dir_of(self.sid)), "")
    }

    /// Whether the agent drew what is on the screen.
    fn is_drawn(&self, screen: &str, seen: &Look, before: &str) -> bool {
        matches!(seen, Look::Ask(_)) || drawn(screen, &launch::start_line(&self.s.dir_of(self.sid)), before)
    }

    /// What a screen still for long is told as; None while there is
    /// nothing of the agent's on it, or nothing to say.  A dialog the
    /// parent was not told of is told whatever it was told before.
    pub(super) fn stop_at(&self, screen: &str, seen: &Look, before: &str) -> Option<Job> {
        let stop = |by_screen| Job::Stopped { sid: self.sid, stop: Stop::Blocked, by_screen };
        match seen {
            Look::Ask(d) => Some(stop(!take_told(&self.s.dir_of(self.sid), d))),
            _ => (self.by_turns() && self.is_drawn(screen, seen, before)).then(|| stop(true)),
        }
    }

    /// Say yes to `d`, which is on the screen: the position of the Enter
    /// and the screen it went to, once the dialog is gone.  One that does
    /// not move when a key goes gets no other, now or later; one whose
    /// key was not sent is tried again at the next pass.
    fn answer(&self, d: &Dialog, keys: &mut Keys) -> Option<(u64, String)> {
        if keys.ended() {
            return None;
        }
        match self.press_through(d, keys, || self.look(), |key| with_port(None, |p| p.send_keys(self.sid, key))) {
            Pressed::Entered(pos, shown) => {
                keys.end();
                self.note(d, "answered");
                Some((pos, shown))
            }
            Pressed::NotSent(why) => {
                keys.unsent();
                self.note(d, &format!("key not sent, {why}"));
                None
            }
            Pressed::Over => {
                keys.end();
                self.note(d, "not answered");
                None
            }
        }
    }

    /// `look`: what is on the screen now; `send`: a key to the pane, None
    /// before there is anything to send it through.
    pub(super) fn press_through(
        &self,
        asked: &Dialog,
        keys: &mut Keys,
        look: impl Fn() -> (String, Look),
        send: impl Fn(&[u8]) -> Option<Result<u64, Reject>>,
    ) -> Pressed {
        let dir = self.s.dir_of(self.sid);
        let mut d = asked.clone();
        while let Some(press) = keys.next(&d) {
            if self.refusal(&d).is_some() || !self.current() {
                return Pressed::Over;
            }
            let (shown, seen) = look();
            if !matches!(&seen, Look::Ask(n) if *n == d) {
                return Pressed::NotSent("the screen changed");
            }
            if press == Press::Enter && !take_enter(&dir, d.ask) {
                return Pressed::Over;
            }
            let pos = match send(press.bytes()) {
                Some(Ok(pos)) => pos,
                // an Enter that may have gone in is not sent again
                Some(Err(r)) if press == Press::Enter && !nothing_sent(r.code) => return Pressed::Over,
                failed => {
                    if press == Press::Enter {
                        give_enter_back(&dir, d.ask);
                    }
                    return Pressed::NotSent(failed.and_then(Result::err).map_or("no socket yet", |r| r.code.as_str()));
                }
            };
            if press == Press::Enter {
                let gone = |l: &Look| !matches!(l, Look::Ask(n) if n.ask == asked.ask);
                return self.until(DRAWN_MS, pos, gone).map_or(Pressed::Over, |_| Pressed::Entered(pos, shown));
            }
            let was = d.cursor;
            match self.until(500, pos, |l| !matches!(l, Look::Ask(n) if n.ask == asked.ask && n.cursor == was)) {
                Some(Look::Ask(n)) if n.ask == asked.ask => d = n,
                _ => return Pressed::Over,
            }
        }
        Pressed::Over
    }

    /// Wait for the pane to print past `pos` and be still, until what it
    /// then shows is `done`.
    fn until(&self, quiet_ms: u32, mut pos: u64, done: impl Fn(&Look) -> bool) -> Option<Look> {
        let end = Instant::now() + KEY_TAKEN;
        loop {
            let waited = with_port(None, |p| p.wait_quiet(self.sid, quiet_ms, Some(pos), end))?;
            let (_, seen) = self.look();
            if done(&seen) {
                return Some(seen);
            }
            match waited {
                Ok(Waited::Met { pos: at }) => pos = at,
                _ => return None,
            }
        }
    }
}

/// The directory a task file names.
pub(super) fn task_dir(task: &str) -> Option<&str> {
    task.lines().find_map(|l| l.strip_prefix("Working directory: "))
}

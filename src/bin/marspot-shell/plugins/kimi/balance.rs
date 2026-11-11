//! Keeping kimi panes on accounts that can answer, by the rules the
//! Claude panes go by.
//!
//! An account is out when the usage feed shows a window spent, or when
//! a pane's turn failed there with the quota error and the window that
//! was about has not renewed.  A pane on such an account moves to
//! another once it is not in the middle of a turn; where to, and when
//! to wait instead, is Claude's `next_move`.  A pane that is working
//! on an account one step from out is told so through its inbox, which
//! kimi's hook hands it when the turn ends.
//!
//! What panes were refused and what they were told is kept on disk: a
//! restart of the shell must not send a pane back to an account that
//! refused one an hour ago.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marspot::cc_usage::CcUsage;
use marspot_peer::registry::{Agent, Status};

use super::{accounts, quota, KimiPlugin, PaneKimi};
use crate::plugins::claudecode::heads_up;
use crate::plugins::claudecode::quota::{next_move, Move, Refusal, Situation};
use crate::plugins::claudecode::rooms::unix_now;
use crate::plugins::claudecode::seen::Seen;
use crate::plugins::{handoff, LogLevel, PluginHost};

const REFUSED_FILE: &str = "refused.tsv";
const HEADS_UP_FILE: &str = "heads_up.tsv";
/// How old the feed and the list of accounts may be.  Both are files;
/// neither is read on every tick.
const READ_EVERY: Duration = Duration::from_secs(10);
/// How long a move is taken to be on its way: the pane reads as being
/// on the account it left until the new kimi is up.
const LANDING: Duration = Duration::from_secs(180);

/// What has been done for a pane while its account has been out.
struct Tally {
    since: Instant,
    moves: u8,
    moved: Option<(Instant, u8)>,
    hold_said: Option<&'static str>,
}

#[derive(Default)]
pub(super) struct Balance {
    refused: Seen,
    told: heads_up::Ledger,
    tallies: HashMap<u64, Tally>,
    usage: Option<CcUsage>,
    /// The accounts a pane can be moved to.
    targets: Vec<(u8, PathBuf)>,
    read_at: Option<Instant>,
}

impl Balance {
    pub(super) fn load(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join(REFUSED_FILE)) {
            self.refused = Seen::decode(&text);
        }
        if let Ok(text) = std::fs::read_to_string(dir.join(HEADS_UP_FILE)) {
            self.told = heads_up::Ledger::decode(&text);
        }
    }

    fn persist(&mut self, host: &dyn PluginHost) {
        for (file, text) in [(REFUSED_FILE, self.refused.changed()), (HEADS_UP_FILE, self.told.changed())] {
            let Some(text) = text else { continue };
            let Ok(dir) = host.state_dir() else { continue };
            if let Err(e) = std::fs::write(dir.join(file), text) {
                host.log(LogLevel::Warn, "quota.persist_failed", &format!("{file}: {e}"));
            }
        }
    }

    fn read(&mut self, now: bool) {
        if !now && self.read_at.is_some_and(|at| at.elapsed() < READ_EVERY) {
            return;
        }
        self.read_at = Some(Instant::now());
        self.usage = marspot::kimi_usage::read();
        self.targets = handoff::profiles(handoff::Agent::Kimi);
    }
}

/// The inbox of the kimi with this pid, from the sessions registry.
fn inbox_of(kimi_pid: i32) -> Option<String> {
    let ours = |e: &marspot_peer::registry::Entry| e.ours && e.agent == Some(Agent::Kimi) && e.agent_pid == Some(kimi_pid);
    marspot_peer::peers::live_entries().into_iter().find(ours)?.socket
}

impl KimiPlugin {
    /// One pass over the kimi panes: take in new refusals, move the
    /// panes whose account is out, tell the working ones whose account
    /// is about to be.
    pub(super) fn keep_on_usable_accounts(&mut self, host: &dyn PluginHost) {
        let now = unix_now();
        let mut news = false;
        for (sid, pane) in &self.panes {
            let Some(failure) = pane.facts.as_ref().and_then(|f| f.failure.as_ref()).filter(|f| quota::is_quota(f)) else { continue };
            let account = pane.profile.unwrap_or(0);
            let window = accounts::refused_window(self.balance.usage.as_ref(), account, now);
            let refusal = Refusal { at: failure.at, model: String::new(), logged_out: false, window };
            if self.balance.refused.note(account, &refusal) {
                host.log(LogLevel::Info, "quota.refused", &format!("sid={sid}: account {account} refused a turn at {}", failure.at));
                news = true;
            }
        }
        if !self.panes.is_empty() {
            self.balance.read(news);
        }
        let panes: Vec<(u64, PaneKimi)> = self.panes.iter().map(|(sid, p)| (*sid, p.clone())).collect();
        for (sid, pane) in panes {
            self.judge(host, sid, &pane, now);
        }
        let b = &mut self.balance;
        b.tallies.retain(|sid, t| self.panes.contains_key(sid) || t.moved.is_some_and(|(at, _)| at.elapsed() < LANDING));
        b.told.retain(|sid, _| self.panes.contains_key(sid));
        b.persist(host);
    }

    fn judge(&mut self, host: &dyn PluginHost, sid: u64, pane: &PaneKimi, now: i64) {
        let b = &mut self.balance;
        let current = pane.profile.unwrap_or(0);
        let mut numbers: Vec<u8> = b.targets.iter().map(|(n, _)| *n).collect();
        if !numbers.contains(&current) {
            numbers.push(current);
        }
        let rooms = accounts::rooms(&numbers, b.usage.as_ref(), &b.refused, now);
        let Some((_, here)) = rooms.iter().find(|(n, _)| *n == current) else { return };
        let status = pane.facts.as_ref().map(|f| f.status);
        let on_its_way = |t: &Tally| t.moved.filter(|(at, to)| *to != current && at.elapsed() < LANDING).map(|(_, to)| to);
        if !here.refused_for(None) {
            if b.tallies.get(&sid).is_some_and(|t| on_its_way(t).is_none()) {
                b.tallies.remove(&sid);
            }
            let working = matches!(status, Some(Status::Busy | Status::Waiting));
            let elsewhere = rooms.iter().any(|(n, r)| *n != current && !r.refused_for(None));
            let week = here.reset_7d();
            if working && elsewhere && heads_up::nearly_out(here, None) && !b.told.get(&sid).is_some_and(|h| h.is_for(current, week)) {
                host.log(LogLevel::Info, "quota.heads_up", &format!("sid={sid}: account {current} is nearly out; telling it"));
                let delivered = Arc::new(AtomicBool::new(false));
                let kimi_pid = pane.proc.pid;
                heads_up::send_to(sid, current, Arc::clone(&delivered), "kimi.quota.heads_up", move || inbox_of(kimi_pid));
                b.told.insert(sid, heads_up::Sent { profile: current, week, delivered });
            }
            return;
        }
        // a turn that is running may yet be answered; one that failed
        // for quota has ended, whatever else the record says
        let failed = pane.facts.as_ref().and_then(|f| f.failure.as_ref()).is_some_and(quota::is_quota);
        if pane.session.is_none() || !(failed || status == Some(Status::Idle)) {
            return;
        }
        let tally = b.tallies.entry(sid).or_insert_with(|| Tally { since: Instant::now(), moves: 0, moved: None, hold_said: None });
        let decision = next_move(&Situation {
            rooms: &rooms,
            current,
            needs_model: None,
            tool_executing: false,
            work_running: false,
            moves_this_episode: tally.moves,
            last_target: on_its_way(tally),
            stranded_for: Some(tally.since.elapsed()),
            since_last_move: tally.moved.map(|(at, _)| at.elapsed()),
            warned: false,
            background_refused: None,
        });
        match decision {
            Move::Hold(why) => {
                if tally.hold_said != Some(why) {
                    tally.hold_said = Some(why);
                    host.log(LogLevel::Info, "quota.stay", &format!("sid={sid}: stays on account {current}, which is out: {why}"));
                }
            }
            Move::To(n) => {
                tally.moves += 1;
                tally.moved = Some((Instant::now(), n));
                tally.hold_said = None;
                let why = if failed { "the account's quota is used up" } else { "the usage feed shows a window spent" };
                host.log(LogLevel::Info, "quota.move", &format!("sid={sid}: account {current} is out ({why}); moving to profile {n}"));
                self.hand_off(host, sid, pane, handoff::Agent::Kimi, n);
            }
        }
    }
}

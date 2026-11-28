//! The codex panes, as each plugin tick sees them, handed to the
//! allocation, and the heads-ups it asks for sent to each session's
//! inbox.  It runs on the tick that already watches the codex panes,
//! so it costs nothing while none is open; the feed is read at most
//! once a minute, and a rollout found by its thread's file name is
//! looked for in three day directories at most every two minutes.
//! What the kept events say is read back on a thread of its own, and
//! nothing is decided until it is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use marspot_peer::registry::Agent;

use super::alloc::{AccountIn, CodexAlloc, PaneIn, Snapshot, events_dir};
use super::alloc_rollout::rollout_of_thread;
use super::credits::{accounts as accounts_of, feed_balances, pane_rate, spent_week, Credit};
use super::rollout::RolloutIndex;
use crate::plugins::claudecode::heads_up;
use crate::plugins::claudecode::quota::Room;
use crate::plugins::claudecode::with_helpers::HelperRoles;
use crate::plugins::usage::for_helpers::codex_accounts;
use crate::plugins::{LogLevel, PluginHost};

/// How often the codex feed is read again.
const FEED_EVERY_S: i64 = 60;

/// A Codex reset credit may be used only while the week it brings back
/// has less than this share left.
const CREDIT_USABLE_BELOW_LEFT: f64 = 0.8;

/// How often a thread whose rollout was not found is looked for again.
const SEARCH_EVERY_S: i64 = 120;

/// Threads remembered with their rollout; far more than panes.
const THREADS_CAP: usize = 256;

/// One codex pane as the tick saw it.
pub(super) struct Seen {
    pub sid: u64,
    pub codex: marspot::pidtree::ProcId,
    pub home: PathBuf,
    pub cwd: Option<String>,
    /// The rollout the badge matched to the pane's working directory.
    pub cwd_rollout: Option<PathBuf>,
    pub model: Option<String>,
    pub profile: Option<u8>,
}

impl Seen {
    pub(super) fn new(sid: u64, codex: &crate::observe::Proc, home: &Path, cwd: Option<String>, rollouts: &RolloutIndex, model: Option<String>, profile: Option<u8>) -> Seen {
        let cwd_rollout = cwd.as_deref().and_then(|c| rollouts.path_for_cwd(c)).cloned();
        Seen { sid, codex: codex.row.id(), home: home.to_path_buf(), cwd, cwd_rollout, model, profile }
    }
}

/// The rollout matched to `s`'s working directory, unless another codex
/// pane works in the same one: the match is by directory, and would
/// read one session's refusals and speed as both panes'.
pub(super) fn cwd_rollout(seen: &[Seen], s: &Seen) -> Option<PathBuf> {
    let shared = seen.iter().any(|o| o.sid != s.sid && o.cwd.is_some() && o.cwd == s.cwd);
    s.cwd_rollout.clone().filter(|_| !shared)
}

/// The allocation, once what the kept events say has been read back.
#[derive(Default)]
enum Core {
    #[default]
    Unread,
    Reading(std::thread::JoinHandle<CodexAlloc>),
    Ready(Box<CodexAlloc>),
}

#[derive(Default)]
pub(super) struct AllocPass {
    core: Core,
    feed: Option<(i64, Option<i64>, Vec<AccountIn>)>,
    /// The feed's credits balances, as `credits::feed_balances` reads them.
    balances: Option<(i64, Vec<(u8, f64)>)>,
    threads: HashMap<String, (i64, Option<PathBuf>)>,
}

impl AllocPass {
    /// One pass; what the credits rule reads of each account, once the
    /// allocation is ready.
    pub(super) fn run(&mut self, host: &dyn PluginHost, seen: &[Seen], obs: &crate::observe::Observation) -> Option<Credit> {
        let now = crate::plugins::claudecode::rooms::unix_now();
        self.core(now)?;
        let views = crate::plugins::peerbus::threads();
        let roles = HelperRoles::of_observation(obs);
        let mut panes = Vec::new();
        for s in seen {
            let Some(profile) = s.profile else { continue };
            let view = views.get(&s.sid).filter(|v| v.codex == s.codex);
            let rollout = match view {
                Some(v) => self.rollout_of(&s.home, &v.thread, s.cwd_rollout.as_deref(), now),
                None => cwd_rollout(seen, s),
            };
            panes.push(PaneIn {
                sid: s.sid,
                profile,
                started_at: (s.codex.start_us / 1_000_000) as i64,
                rollout,
                idle: view.is_some_and(|v| v.idle),
                helper: roles.is_helper(s.sid),
                restricted: s.model.as_deref().is_some_and(|m| m.to_ascii_lowercase().contains("spark")),
                peer: view.map(|v| v.ends.clone()).unwrap_or_default(),
            });
        }
        let (feed_at, accounts) = self.feed(now);
        let balances = self.balances.clone();
        let alloc = self.core(now)?;
        let snap = Snapshot { now, feed_at, accounts, panes };
        let (log, tell) = alloc.observe(&snap);
        if let Some((at, list)) = balances {
            for (n, b) in list {
                let spent = snap.accounts.iter().find(|a| a.profile == n).and_then(|a| spent_week(a, now));
                alloc.credit_seen.saw(n, at, b, spent);
            }
        }
        let rates = snap.panes.iter().map(|p| (p.sid, pane_rate(alloc, p, now))).collect();
        let credit = Credit { now, accts: accounts_of(&snap, &alloc.limits, &alloc.credit_seen), rates };
        for (tag, line) in log {
            host.log(LogLevel::Info, tag, &line);
        }
        for t in tell {
            let Some(s) = seen.iter().find(|s| s.sid == t.sid) else { continue };
            host.log(LogLevel::Info, "codex.quota.heads_up", &format!("sid={} P{} lasts {:.1} h; telling it", t.sid, t.profile, t.hours));
            let pid = s.codex.pid;
            let body = body(t.profile, t.hours);
            heads_up::send_to(t.sid, t.profile, body, Arc::new(AtomicBool::new(false)), "codex.quota.heads_up", move || inbox_of(pid));
        }
        Some(credit)
    }

    /// The allocation, or None while the kept events are still being read.
    fn core(&mut self, now: i64) -> Option<&mut CodexAlloc> {
        if matches!(&self.core, Core::Reading(h) if h.is_finished())
            && let Core::Reading(h) = std::mem::take(&mut self.core)
        {
            self.core = Core::Ready(Box::new(h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))));
        }
        if matches!(self.core, Core::Unread) {
            let dir = events_dir(&marspot::paths::state_root());
            let read = move || {
                let mut a = CodexAlloc::new(dir);
                a.rebuild(now);
                a
            };
            self.core = Core::Reading(std::thread::Builder::new().name("codex-alloc-read".into()).spawn(read).expect("spawn the codex allocation's reader"));
        }
        match &mut self.core {
            Core::Ready(a) => Some(a.as_mut()),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn ready(&mut self) -> bool {
        self.core(crate::plugins::claudecode::rooms::unix_now()).is_some()
    }

    /// The pane's rollout by its thread: the one matched by directory
    /// when its name is that thread's, else found by name.
    fn rollout_of(&mut self, home: &Path, thread: &str, by_cwd: Option<&Path>, now: i64) -> Option<PathBuf> {
        let suffix = format!("-{thread}.jsonl");
        if let Some(p) = by_cwd.filter(|p| p.to_string_lossy().ends_with(&suffix)) {
            return Some(p.to_path_buf());
        }
        if let Some((at, found)) = self.threads.get(thread)
            && (found.is_some() || now - at < SEARCH_EVERY_S)
        {
            return found.clone();
        }
        if self.threads.len() >= THREADS_CAP {
            self.threads.clear();
        }
        let found = rollout_of_thread(home, thread);
        self.threads.insert(thread.to_string(), (now, found.clone()));
        found
    }

    fn feed(&mut self, now: i64) -> (Option<i64>, Vec<AccountIn>) {
        if let Some((at, feed_at, accounts)) = &self.feed
            && now - at < FEED_EVERY_S
        {
            return (*feed_at, accounts.clone());
        }
        let numbers: Vec<u8> = super::profile_dirs().into_iter().map(|(n, _)| n).collect();
        use marspot::cc_usage as usage;
        let body = usage::authoritative_body(&usage::codex_feed_path(), &usage::own_codex_feed_path(), now);
        self.balances = body.as_deref().and_then(feed_balances);
        let read = codex_accounts(&numbers, body.as_deref().and_then(usage::parse_codex), now);
        let resets = marspot::agent_resets::read(now);
        let feed_at = (read.feed_at > 0).then_some(read.feed_at);
        let accounts: Vec<AccountIn> = read
            .rooms
            .iter()
            .filter_map(|(n, room)| {
                let spark_at = read.resets.iter().find(|(m, _)| m == n).and_then(|(_, r)| r.models.iter().find(|(l, _)| l == "spark").map(|(_, at)| *at));
                let mut ac = account_in(*n, room, spark_at)?;
                ac.supply = crate::agentsvc::sched::supply::of_account(&resets, "codex", *n, Some(CREDIT_USABLE_BELOW_LEFT), now);
                Some(ac)
            })
            .collect();
        self.feed = Some((now, feed_at, accounts.clone()));
        (feed_at, accounts)
    }
}

/// An account the feed reads a week for; an unread one is no target.
fn account_in(profile: u8, room: &Room, spark_at: Option<i64>) -> Option<AccountIn> {
    let Room::Known { util_7d: Some(w), reset_7d, models, measured_at, .. } = room else { return None };
    let spark = models.iter().find(|(l, _)| l == "spark").and_then(|(_, u)| *u).zip(spark_at);
    Some(AccountIn { profile, week_used: *w, week_renews: *reset_7d, spark, open: !room.refused_for(None), measured_at: *measured_at, supply: Vec::new() })
}

/// The inbox the peerbus relays to this codex.
fn inbox_of(codex_pid: i32) -> Option<String> {
    let ours = |e: &marspot_peer::registry::Entry| e.ours && e.agent == Some(Agent::Codex) && e.agent_pid == Some(codex_pid);
    marspot_peer::peers::live_entries().into_iter().find(ours)?.socket
}

/// Said to a busy codex session whose account will not last.  Codex is
/// not moved to another account by marspot, so the session is not
/// promised a switch: it is asked to keep working and to write down
/// what runs in the background, since a refusal stops its turn.
pub(super) fn body(profile: u8, hours: f64) -> String {
    format!(
        "marspot 提醒：这个会话所用的 Codex 账号（P{profile}）按现在的用量撑不到 2 小时（约 {hours:.1} 小时）。\
         请照常继续手头的任务，不要因为这条提醒停下或者收尾，在额度用完之前这个账号还能正常使用。\
         额度用完后这一轮会被拒绝，要等用户换账号或者额度恢复才能接着做，所以正在运行的和以后启动的后台任务，请把命令和输出位置记下来。\
         这条消息不用回复。"
    )
}

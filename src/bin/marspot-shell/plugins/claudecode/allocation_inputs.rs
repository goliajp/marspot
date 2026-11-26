//! What the allocation core is told about Claude, from one scan.
//!
//! Accounts: the week and the Fable cap as the feed reads them, a
//! renewal that has passed read as a fresh window.  An account the feed
//! does not cover, or covers with no week reading, is no target and is
//! left out; one whose Fable cap is listed with no reading is no
//! target for Fable.  Panes: bound ones with a profile of their own,
//! judged as Fable when running it or while the main thread has it
//! marked as needing it.  The marks are the main thread's own, read
//! from the file it keeps them in: the scan worker shares no memory
//! with it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use super::fable_need::{self, MARK_FOR, Marks};
use super::quota::{self, Room};
use super::{BindMeta, CcActivity, ScanResult, SessionInfo, rooms};
use crate::agentsvc::sched::supply;
use crate::plugins::allocation::{Class, KAPPA_FABLE, Load, Quant};
use marspot_agent::sched::reset::{Supply, with_supply};

/// A reading older than this is extrapolated and ranked after fresh ones.
pub(super) const STALE_ACCOUNT_S: i64 = 30 * 60;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AccountIn {
    pub profile: u8,
    pub week_used: f64,
    /// The Fable cap's reading; `None` when the feed gives none.
    pub fable_used: Option<f64>,
    /// The Fable cap is read, or there is none: not listed with no reading.
    pub fable_known: bool,
    pub week_renews: i64,
    pub fable_renews: i64,
    pub open: bool,
    pub measured_at: i64,
    /// What its stored resets bring back before they end.
    pub supply: Vec<Supply>,
}

impl AccountIn {
    /// The core's view as of `now`, the reading pushed forward by
    /// `rate_since` (weeks an hour) when it is old.
    pub(super) fn quant(&self, now: i64, rate_since: f64) -> Quant {
        let age = now - self.measured_at;
        let stale = age > STALE_ACCOUNT_S;
        let w = if stale { self.week_used + rate_since * age as f64 / 3600.0 } else { self.week_used };
        let f = if self.fable_known { KAPPA_FABLE * (1.0 - self.fable_used.unwrap_or(0.0)).max(0.0) } else { 0.0 };
        let r = (1.0 - w).max(0.0);
        let hours = |at: i64| (at - now) as f64 / 3600.0;
        let q = Quant { id: self.profile, r, f, t: hours(self.week_renews), tf: hours(self.fable_renews), open: self.open, stale };
        with_supply(&q, &Load { restricted: 0.0, plain: rate_since }, KAPPA_FABLE, &self.supply)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PaneIn {
    pub sid: u64,
    pub profile: u8,
    pub class: Class,
    /// When the main thread marked it as needing Fable, while marked.
    pub marked_at: Option<i64>,
    /// Its last refusal of any kind, unix seconds.
    pub refused_at: Option<i64>,
    pub resting: bool,
    pub helper: bool,
    /// Its account cannot answer it now.
    pub out: bool,
    pub transcript: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Snapshot {
    pub now: i64,
    /// When the feed was written; `None` when there is no feed.
    pub feed_at: Option<i64>,
    pub accounts: Vec<AccountIn>,
    pub panes: Vec<PaneIn>,
    /// Every live pane, bound or not.
    pub live: Vec<u64>,
}

pub(super) fn snapshot(result: &ScanResult, seen: &HashMap<PathBuf, SessionInfo>, marks: &HashMap<u64, i64>, now: i64) -> Snapshot {
    let Some(usage) = result.usage.as_ref() else {
        return Snapshot { now, live: result.sessions_seen.clone(), ..Snapshot::default() };
    };
    let email = |p: u8| result.profiles.iter().find(|(n, _)| *n == p).and_then(|(_, e)| e.clone());
    let accounts = accounts_of(&result.profiles, usage, now);
    let rooms: Vec<(u8, Room)> = rooms::rooms_for(&result.profiles.iter().map(|(n, _)| *n).collect::<Vec<_>>(), &usage.accounts, email, now);
    let mut panes: Vec<PaneIn> = result.new_meta.iter().filter_map(|(sid, m)| pane_in(*sid, m, result, seen, marks, &rooms, now)).collect();
    panes.sort_by_key(|p| p.sid);
    Snapshot { now, feed_at: Some(usage.generated_at), accounts, panes, live: result.sessions_seen.clone() }
}

/// Each numbered profile's account as the feed reads it, `profiles`
/// pairing each with its address.
pub(super) fn accounts_of(profiles: &[(u8, Option<String>)], usage: &marspot::cc_usage::CcUsage, now: i64) -> Vec<AccountIn> {
    let resets = marspot::agent_resets::read(now);
    profiles
        .iter()
        .filter_map(|(p, mail)| {
            let a = usage.accounts.iter().find(|a| Some(&a.email) == mail.as_ref())?;
            let fable_reset = a.model_limits.iter().find(|m| quota::same_model("fable", &m.label.to_ascii_lowercase())).and_then(|m| m.reset);
            let mut ac = account_in(*p, &rooms::room_of(a, now), fable_reset.filter(|r| *r > now))?;
            ac.supply = supply::of_account(&resets, "claude", *p, None, now);
            Some(ac)
        })
        .collect()
}

fn account_in(profile: u8, room: &Room, fable_renews: Option<i64>) -> Option<AccountIn> {
    let Room::Known { util_7d: Some(w), reset_7d, models, measured_at, .. } = room else { return None };
    let fable_used = models.iter().find(|(l, _)| quota::same_model("fable", l)).and_then(|(_, u)| *u);
    Some(AccountIn {
        profile,
        week_used: *w,
        fable_used,
        fable_known: room.known_for(Some(fable_need::FABLE)),
        week_renews: *reset_7d,
        fable_renews: fable_renews.unwrap_or(*reset_7d),
        open: !room.refused_for(None),
        measured_at: *measured_at,
        supply: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)] // one pane's view of the whole scan
fn pane_in(sid: u64, m: &BindMeta, result: &ScanResult, seen: &HashMap<PathBuf, SessionInfo>, marks: &HashMap<u64, i64>, rooms: &[(u8, Room)], now: i64) -> Option<PaneIn> {
    m.config_dir.as_ref()?;
    let model = result.new_models.get(&sid).map(String::as_str);
    let marked_at = marks.get(&sid).copied().filter(|since| now - since < MARK_FOR.as_secs() as i64);
    let marked = marked_at.is_some();
    let class = if marked || quota::needs_fable(model) { Class::Restricted } else { Class::Plain };
    let judged = if class == Class::Restricted { Some("fable") } else { model };
    let out = rooms.iter().find(|(p, _)| *p == m.profile_num).is_some_and(|(_, r)| r.refused_for_now(judged, m.refused_at.as_ref()));
    Some(PaneIn {
        sid,
        profile: m.profile_num,
        class,
        marked_at,
        refused_at: m.refused_at.as_ref().map(|r| r.at),
        resting: matches!(result.new_activity.get(&sid), Some(CcActivity::AwaitingUser)),
        helper: result.helpers.is_helper(sid),
        out,
        transcript: seen.values().find(|s| s.session_id == m.uuid).map(|s| s.jsonl_path.clone()),
    })
}

/// The time in a fact's `at=<unix>` detail.
pub(super) fn at_of(detail: &str) -> Option<i64> {
    detail.strip_prefix("at=")?.parse().ok()
}

/// The main thread's `fable_need` file, read again when it changes.
#[derive(Debug)]
pub(super) struct MarksFile {
    path: PathBuf,
    mtime: Option<SystemTime>,
    marks: HashMap<u64, i64>,
}

impl MarksFile {
    pub(super) fn new(state_dir: PathBuf) -> Self {
        MarksFile { path: state_dir.join(fable_need::FILE), mtime: None, marks: HashMap::new() }
    }

    /// Each marked pane and since when, as of `now`.
    pub(super) fn read(&mut self, now: i64) -> &HashMap<u64, i64> {
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if mtime != self.mtime {
            self.mtime = mtime;
            let text = std::fs::read_to_string(&self.path).unwrap_or_default();
            self.marks = Marks::decode(&text, now).since_by_pane().collect();
        }
        &self.marks
    }
}

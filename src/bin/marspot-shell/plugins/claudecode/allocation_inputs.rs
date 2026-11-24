//! What the allocation core is told about Claude, from one scan.
//!
//! Accounts: the week and the Fable cap as the feed reads them, a
//! renewal that has passed read as a fresh window.  An account the feed
//! does not cover, or covers with no week reading, is no target and is
//! left out.  Panes: bound ones with a profile of their own, judged as
//! Fable when running it or refused it in the last twelve hours.

use std::collections::HashMap;
use std::path::PathBuf;

use super::quota::{self, Room};
use super::{BindMeta, CcActivity, ScanResult, SessionInfo, rooms};
use crate::plugins::allocation::{Class, KAPPA_FABLE, Quant};

/// A pane refused Fable is judged as a Fable pane this long.
pub(super) const MARKED_FOR_S: i64 = 12 * 3600;

/// A reading older than this is extrapolated and ranked after fresh ones.
pub(super) const STALE_ACCOUNT_S: i64 = 30 * 60;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AccountIn {
    pub profile: u8,
    pub week_used: f64,
    /// The Fable cap's reading; `None` when the feed gives none.
    pub fable_used: Option<f64>,
    pub week_renews: i64,
    pub fable_renews: i64,
    pub open: bool,
    pub measured_at: i64,
}

impl AccountIn {
    /// The core's view as of `now`, the reading pushed forward by
    /// `rate_since` (weeks an hour) when it is old.
    pub(super) fn quant(&self, now: i64, rate_since: f64) -> Quant {
        let age = now - self.measured_at;
        let stale = age > STALE_ACCOUNT_S;
        let w = if stale { self.week_used + rate_since * age as f64 / 3600.0 } else { self.week_used };
        let f = self.fable_used.unwrap_or(0.0);
        let r = (1.0 - w).max(0.0);
        let hours = |at: i64| (at - now) as f64 / 3600.0;
        Quant { id: self.profile, r, f: KAPPA_FABLE * (1.0 - f).max(0.0), t: hours(self.week_renews), tf: hours(self.fable_renews), open: self.open, stale }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PaneIn {
    pub sid: u64,
    pub profile: u8,
    pub class: Class,
    /// Its last Fable refusal, unix seconds.
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

pub(super) fn snapshot(result: &ScanResult, seen: &HashMap<PathBuf, SessionInfo>, now: i64) -> Snapshot {
    let Some(usage) = result.usage.as_ref() else {
        return Snapshot { now, live: result.sessions_seen.clone(), ..Snapshot::default() };
    };
    let email = |p: u8| result.profiles.iter().find(|(n, _)| *n == p).and_then(|(_, e)| e.clone());
    let accounts = result
        .profiles
        .iter()
        .filter_map(|(p, _)| {
            let mail = email(*p)?;
            let a = usage.accounts.iter().find(|a| a.email == mail)?;
            let fable_reset = a.model_limits.iter().find(|m| quota::same_model("fable", &m.label.to_ascii_lowercase())).and_then(|m| m.reset);
            account_in(*p, &rooms::room_of(a, now), fable_reset.filter(|r| *r > now))
        })
        .collect();
    let rooms: Vec<(u8, Room)> = rooms::rooms_for(&result.profiles.iter().map(|(n, _)| *n).collect::<Vec<_>>(), &usage.accounts, email, now);
    let mut panes: Vec<PaneIn> = result.new_meta.iter().filter_map(|(sid, m)| pane_in(*sid, m, result, seen, &rooms, now)).collect();
    panes.sort_by_key(|p| p.sid);
    Snapshot { now, feed_at: Some(usage.generated_at), accounts, panes, live: result.sessions_seen.clone() }
}

fn account_in(profile: u8, room: &Room, fable_renews: Option<i64>) -> Option<AccountIn> {
    let Room::Known { util_7d: Some(w), reset_7d, models, measured_at, .. } = room else { return None };
    let fable_used = models.iter().find(|(l, _)| quota::same_model("fable", l)).and_then(|(_, u)| *u);
    Some(AccountIn {
        profile,
        week_used: *w,
        fable_used,
        week_renews: *reset_7d,
        fable_renews: fable_renews.unwrap_or(*reset_7d),
        open: !room.refused_for(None),
        measured_at: *measured_at,
    })
}

fn pane_in(sid: u64, m: &BindMeta, result: &ScanResult, seen: &HashMap<PathBuf, SessionInfo>, rooms: &[(u8, Room)], now: i64) -> Option<PaneIn> {
    m.config_dir.as_ref()?;
    let model = result.new_models.get(&sid).map(String::as_str);
    let marked_at = result.fable_refused.get(&sid).map(|a| a.at);
    let marked = marked_at.is_some_and(|at| now - at < MARKED_FOR_S);
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

//! Who leaves an account running short, who stays, and where each one
//! goes: decided from one scan's facts, touching nothing.

use std::collections::HashMap;

use marspot::cc_usage::CcAccount;

use super::burn::{Load, Outlook, Speeds};
use super::{heads_up, quota};

/// The account a pane leaves for has to last this long with it.
pub(in crate::plugins) const TARGET_H: f64 = 6.0;
/// What a pane is taken to use where nothing is measured: 0.6 points
/// of a week an hour, the week's demand over its dozen panes.
pub(in crate::plugins) const RHO0: f64 = 0.006;
/// A pane told its account is short is told again only once the
/// account has been seen to last this long.
pub(in crate::plugins) const RECOVERED_H: f64 = 4.0;

/// One pane's part in this pass.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(in crate::plugins) struct Part {
    pub profile: u8,
    pub short: bool,
    /// The slowest pane on a short account: it does not leave.
    pub stays: bool,
    /// Share of a week per hour.
    pub speed: f64,
    /// How long its account lasts, where a speed is known.
    pub hours: Option<f64>,
}

/// A pane taking part: session, profile, the model it is judged as.
pub(in crate::plugins) type Taking<'a> = (u64, u8, Option<&'a str>);

/// Everything one pass decides by.
pub(in crate::plugins) struct Plan<'a> {
    speeds: &'a Speeds,
    pub(super) parts: HashMap<u64, Part>,
    /// Panes on each account taking part, all and on Fable.
    on: HashMap<u8, (usize, usize)>,
    /// What the panes sent this pass add to each account.
    placed: HashMap<u8, Load>,
    rooms: Vec<(u8, quota::Room)>,
    accounts: Vec<(u8, CcAccount)>,
    now: i64,
}

/// What a pane of `speed` on `model` adds to an account.
fn load_of(model: Option<&str>, speed: f64) -> Load {
    Load { week: speed, fable: if quota::needs_fable(model) { speed } else { 0.0 } }
}

/// Who leaves and who stays, for every pane taking part. A pane's speed
/// is its account's, shared by the tokens each pane's responses
/// weighed over the last ninety minutes.
pub(in crate::plugins) fn plan<'a>(
    speeds: &'a Speeds,
    rooms: &[(u8, quota::Room)],
    feed: &[CcAccount],
    profiles: &[(u8, Option<String>)],
    panes: &[Taking],
    tokens: &HashMap<u64, f64>,
    now: i64,
) -> Plan<'a> {
    let accounts: Vec<(u8, CcAccount)> = profiles
        .iter()
        .filter_map(|(p, mail)| Some((*p, feed.iter().find(|a| Some(&a.email) == mail.as_ref())?.clone())))
        .collect();
    let mut plan = Plan { speeds, parts: HashMap::new(), on: HashMap::new(), placed: HashMap::new(), rooms: rooms.to_vec(), accounts, now };
    let mut used: HashMap<u8, f64> = HashMap::new();
    for (sid, p, model) in panes {
        let on = plan.on.entry(*p).or_default();
        on.0 += 1;
        on.1 += usize::from(quota::needs_fable(*model));
        *used.entry(*p).or_default() += tokens.get(sid).copied().unwrap_or(0.0);
    }
    let mut slowest: HashMap<u8, (f64, u64)> = HashMap::new();
    for (sid, p, model) in panes {
        let n = plan.on[p].0 as f64;
        let mine = tokens.get(sid).copied().unwrap_or(0.0);
        let share = if used[p] > 0.0 { mine / used[p] } else { 1.0 / n };
        let mut part = Part { profile: *p, speed: plan.week_speed(*p).unwrap_or(RHO0 * n) * share, ..Part::default() };
        if let Some(o) = plan.outlook(*p) {
            part.short = o.short(*model);
            part.hours = o.hours_left(*model, Load::default(), None);
        }
        if part.short && slowest.get(p).is_none_or(|s| (mine, *sid) < *s) {
            slowest.insert(*p, (mine, *sid));
        }
        plan.parts.insert(*sid, part);
    }
    for (_, sid) in slowest.values() {
        if let Some(part) = plan.parts.get_mut(sid) {
            part.stays = true;
        }
    }
    plan
}

impl Plan<'_> {
    pub(in crate::plugins) fn part(&self, sid: u64) -> Part {
        self.parts.get(&sid).copied().unwrap_or_default()
    }

    /// A pane's speed; one that takes no part, such as a refused one,
    /// is taken to use the default.
    pub(in crate::plugins) fn speed_of(&self, sid: u64) -> f64 {
        self.parts.get(&sid).map_or(RHO0, |p| p.speed)
    }

    pub(in crate::plugins) fn rooms(&self) -> &[(u8, quota::Room)] {
        &self.rooms
    }

    fn outlook(&self, profile: u8) -> Option<Outlook<'_>> {
        let room = self.rooms.iter().find(|(p, _)| *p == profile).map(|(_, r)| r)?;
        let account = self.accounts.iter().find(|(p, _)| *p == profile).map(|(_, a)| a)?;
        Some(Outlook { speeds: self.speeds, account, room, now: self.now })
    }

    fn week_speed(&self, profile: u8) -> Option<f64> {
        let account = self.accounts.iter().find(|(p, _)| *p == profile).map(|(_, a)| a)?;
        self.speeds.per_hour(&account.email, "7d")
    }

    /// How long `profile` lasts for a pane on `model` using `speed` more.
    pub(in crate::plugins) fn lasts(&self, profile: u8, model: Option<&str>, speed: f64) -> f64 {
        let Some(o) = self.outlook(profile) else { return 0.0 };
        let placed = self.placed.get(&profile).copied().unwrap_or_default();
        let mine = load_of(model, speed);
        let added = Load { week: placed.week + mine.week, fable: placed.fable + mine.fable };
        let (n, fable) = self.on.get(&profile).copied().unwrap_or_default();
        let unknown = Load { week: RHO0 * n as f64, fable: RHO0 * fable as f64 };
        o.hours_left(model, added, Some(unknown)).unwrap_or(0.0)
    }

    /// The accounts a pane on `current` may be sent to: its own, and
    /// those lasting `TARGET_H` with it.
    pub(in crate::plugins) fn lasting(&self, current: u8, model: Option<&str>, speed: f64) -> Vec<(u8, quota::Room)> {
        self.rooms
            .iter()
            .filter(|(p, _)| *p == current || self.lasts(*p, model, speed) >= TARGET_H)
            .cloned()
            .collect()
    }

    /// Where a pane on `current` goes: the account `rank` puts first
    /// among the others lasting `TARGET_H` with it that have room for it.
    pub(in crate::plugins) fn target(&self, current: u8, model: Option<&str>, speed: f64, rank: impl Fn(&[u8]) -> Option<u8>) -> Option<(u8, f64)> {
        let lasting: Vec<u8> = self
            .lasting(current, model, speed)
            .iter()
            .filter(|(p, r)| *p != current && !r.refused_for(model) && r.known_for(model) && !heads_up::nearly_out(r, model))
            .map(|(p, _)| *p)
            .collect();
        let to = rank(&lasting).filter(|to| lasting.contains(to))?;
        Some((to, self.lasts(to, model, speed)))
    }

    /// A pane of `speed` on `model` was sent to `profile` this pass.
    pub(in crate::plugins) fn place(&mut self, profile: u8, model: Option<&str>, speed: f64) {
        let mine = load_of(model, speed);
        let p = self.placed.entry(profile).or_default();
        p.week += mine.week;
        p.fable += mine.fable;
    }
}

#[cfg(test)]
#[path = "leave_plan_tests.rs"]
mod tests;

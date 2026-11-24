//! Leaving an account that cannot last two hours at the speed it is
//! used: every resting pane on it goes to an account that lasts six
//! hours with it, except the slowest, which stays to use what is left.
//!
//! Waiting for the refusal moved every pane at once: on 2026-11-24 at
//! 03:20:53 seven panes left P3 in the same second, most of them in a
//! turn. Sending every pane away early instead leaves the last of the
//! account to lapse at its renewal with nobody on it.
//!
//! This is a move a pane cannot do without, so it is not held by the
//! half hour between ordinary moves, by their one per pass, nor by the
//! switch that turns ordinary moves off or to a dry run.

use super::burn::{Outlook, Trends, SHORT_H};
use super::*;
use marspot::cc_usage::CcAccount;

/// The account a pane leaves for has to last this long with it.
pub(super) const TARGET_H: f64 = 6.0;
/// What a pane is taken to use where nothing is measured: 0.6 points
/// of a week an hour, the week's demand over its dozen panes.
pub(super) const RHO0: f64 = 0.006;
/// A pane told its account is short is told again only once the
/// account has been seen to last this long.
pub(super) const RECOVERED_H: f64 = 4.0;

/// What persists of this between passes.
#[derive(Default)]
pub(super) struct State {
    pub(super) trends: Trends,
    /// Accounts last logged as short, with the pane staying on each.
    said: HashMap<u8, Option<u64>>,
}

/// One pane's part in this pass.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Part {
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
pub(super) type Taking<'a> = (u64, u8, Option<&'a str>);

/// Everything one pass decides by.
pub(super) struct Plan {
    parts: HashMap<u64, Part>,
    on: HashMap<u8, usize>,
    /// What the panes sent this pass add to each account.
    placed: HashMap<u8, f64>,
    rooms: Vec<(u8, quota::Room)>,
    accounts: Vec<(u8, CcAccount)>,
    now: i64,
}

/// Who leaves and who stays, for every pane taking part. A pane's speed
/// is its account's, shared by the tokens each pane's responses
/// weighed over the last ninety minutes.
pub(super) fn plan(
    trends: &Trends,
    rooms: &[(u8, quota::Room)],
    feed: &[CcAccount],
    profiles: &[(u8, Option<String>)],
    panes: &[Taking],
    tokens: &HashMap<u64, f64>,
    now: i64,
) -> Plan {
    let accounts: Vec<(u8, CcAccount)> = profiles
        .iter()
        .filter_map(|(p, mail)| Some((*p, feed.iter().find(|a| Some(&a.email) == mail.as_ref())?.clone())))
        .collect();
    let mut plan = Plan { parts: HashMap::new(), on: HashMap::new(), placed: HashMap::new(), rooms: rooms.to_vec(), accounts, now };
    let mut used: HashMap<u8, f64> = HashMap::new();
    for (sid, p, _) in panes {
        *plan.on.entry(*p).or_default() += 1;
        *used.entry(*p).or_default() += tokens.get(sid).copied().unwrap_or(0.0);
    }
    let mut slowest: HashMap<u8, (f64, u64)> = HashMap::new();
    for (sid, p, model) in panes {
        let n = plan.on[p] as f64;
        let mine = tokens.get(sid).copied().unwrap_or(0.0);
        let share = if used[p] > 0.0 { mine / used[p] } else { 1.0 / n };
        let mut part = Part { profile: *p, speed: plan.week_speed(trends, *p).unwrap_or(RHO0 * n) * share, ..Part::default() };
        if let Some(o) = plan.outlook(trends, *p) {
            part.short = o.short(*model);
            part.hours = o.hours_left(*model, 0.0, None);
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

impl Plan {
    pub(super) fn part(&self, sid: u64) -> Part {
        self.parts.get(&sid).copied().unwrap_or_default()
    }

    fn outlook<'a>(&'a self, trends: &'a Trends, profile: u8) -> Option<Outlook<'a>> {
        let room = self.rooms.iter().find(|(p, _)| *p == profile).map(|(_, r)| r)?;
        let account = self.accounts.iter().find(|(p, _)| *p == profile).map(|(_, a)| a)?;
        Some(Outlook { trends, account, room, now: self.now })
    }

    fn week_speed(&self, trends: &Trends, profile: u8) -> Option<f64> {
        let account = self.accounts.iter().find(|(p, _)| *p == profile).map(|(_, a)| a)?;
        trends.per_hour(&account.email, "7d")
    }

    /// How long `profile` lasts for a pane on `model` using `speed` more.
    fn lasts(&self, trends: &Trends, profile: u8, model: Option<&str>, speed: f64) -> f64 {
        let Some(o) = self.outlook(trends, profile) else { return 0.0 };
        let added = self.placed.get(&profile).copied().unwrap_or(0.0) + speed;
        let unknown = RHO0 * self.on.get(&profile).copied().unwrap_or(0) as f64;
        o.hours_left(model, added, Some(unknown)).unwrap_or(0.0)
    }

    /// Where a pane on `current` goes: the account the usual ranking
    /// puts first among those lasting `TARGET_H` with it.
    pub(super) fn target(&self, trends: &Trends, current: u8, model: Option<&str>, speed: f64) -> Option<(u8, f64)> {
        let lasting: Vec<(u8, quota::Room)> = self
            .rooms
            .iter()
            .filter(|(p, _)| *p == current || self.lasts(trends, *p, model, speed) >= TARGET_H)
            .cloned()
            .collect();
        let to = quota::best_profile(&lasting, current, model)?;
        let room = &lasting.iter().find(|(p, _)| *p == to)?.1;
        if room.refused_for(model) || !room.known_for(model) || heads_up::nearly_out(room, model) {
            return None;
        }
        Some((to, self.lasts(trends, to, model, speed)))
    }

    pub(super) fn place(&mut self, profile: u8, speed: f64) {
        *self.placed.entry(profile).or_default() += speed;
    }
}

fn hours(h: Option<f64>) -> String {
    match h {
        Some(h) if h.is_finite() => format!("{h:.1}h"),
        Some(_) => "no end".into(),
        None => "one step from out, speed not yet known".into(),
    }
}

/// The readings the speeds are measured from, so a restart of the
/// shell -- every silent update is one -- does not wait half an hour
/// for a speed again.
const TRENDS_FILE: &str = "usage_trend.tsv";

impl ClaudecodePlugin {
    pub(super) fn load_trends(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = fs::read_to_string(dir.join(TRENDS_FILE)) {
            self.leave.trends = Trends::decode(&text);
        }
    }

    pub(super) fn persist_trends(&mut self, host: &dyn PluginHost) {
        let Some(text) = self.leave.trends.changed() else { return };
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = fs::write(dir.join(TRENDS_FILE), text) {
            host.log(LogLevel::Warn, "quota.trend_persist_failed", &format!("{e}"));
        }
    }

    /// One line when an account starts or stops running short, and when
    /// the pane staying on it changes.
    pub(super) fn say_short(&mut self, host: &dyn PluginHost, plan: &Plan) {
        let mut now: HashMap<u8, (Option<u64>, Option<f64>, usize)> = HashMap::new();
        for (sid, part) in plan.parts.iter().filter(|(_, p)| p.short) {
            let e = now.entry(part.profile).or_insert((None, part.hours, 0));
            e.2 += 1;
            if part.stays {
                e.0 = Some(*sid);
            }
        }
        for (profile, (stays, h, n)) in &now {
            if self.leave.said.get(profile) != Some(stays) {
                let who = stays.map_or("none".into(), |s| format!("sid={s}"));
                let line = format!("P{profile} lasts {} at its speed, under {SHORT_H}h; {n} panes on it, {who} stays to use the rest", hours(*h));
                host.log(LogLevel::Info, "quota.short", &line);
                self.leave.said.insert(*profile, *stays);
            }
        }
        let over: Vec<u8> = self.leave.said.keys().copied().filter(|p| !now.contains_key(p)).collect();
        for p in over {
            self.leave.said.remove(&p);
            host.log(LogLevel::Info, "quota.short_over", &format!("P{p} lasts {SHORT_H}h or more again"));
        }
    }

    /// Move a resting pane off an account that is running short.
    /// True when a move was started.
    #[allow(clippy::too_many_arguments)] // one call site, each argument a separate fact
    pub(super) fn leave_short(
        &mut self,
        host: &dyn PluginHost,
        sid: u64,
        meta: &BindMeta,
        result: &ScanResult,
        model: Option<&str>,
        choice: &CliChoice,
        move_in_flight: bool,
        plan: &mut Plan,
    ) -> bool {
        if move_in_flight {
            return false;
        }
        let part = plan.part(sid);
        let from = meta.profile_num;
        let Some((to, lasts)) = plan.target(&self.leave.trends, from, model, part.speed) else {
            self.say_once(host, sid, "leave|none", "quota.leave_nowhere", format!("sid={sid} on P{from}: no account lasts {TARGET_H}h with it"));
            return false;
        };
        let pane = self.resting(host, sid, meta, result, move_in_flight, None);
        let held = rebalance::not_now(&pane);
        let why = format!("P{from} lasts {}, P{to} {} with it", hours(part.hours), hours(Some(lasts)));
        if let Some(reason) = held {
            let line = format!("sid={sid} P{from} → P{to} ({why}) waits: {reason}");
            self.say_once(host, sid, &format!("leave|{to}|held"), "quota.leave_waits", line);
            return false;
        }
        host.log(LogLevel::Info, "quota.leaving", &format!("sid={sid} P{from} → P{to}: {why}"));
        self.rebalance_said.insert(sid, format!("leave|{to}|moving"));
        let warned = self.heads_up.get(&sid).is_some_and(|h| h.landed_on(from));
        let word = warned.then(|| heads_up::after_warned_move(false));
        if self.start_profile_cycle_to(host, sid, meta.clone(), to, word, choice.clone()) {
            self.rebalanced.insert(sid, (Instant::now(), to));
            plan.place(to, part.speed);
            return true;
        }
        false
    }

    fn say_once(&mut self, host: &dyn PluginHost, sid: u64, key: &str, event: &str, line: String) {
        if self.rebalance_said.get(&sid).map(String::as_str) != Some(key) {
            self.rebalance_said.insert(sid, key.to_string());
            host.log(LogLevel::Info, event, &line);
        }
    }

    /// What is known about a pane that a move away from rest checks.
    pub(super) fn resting(
        &self,
        host: &dyn PluginHost,
        sid: u64,
        meta: &BindMeta,
        result: &ScanResult,
        move_in_flight: bool,
        since_last: Option<Duration>,
    ) -> rebalance::Pane {
        let (work_in_flight, own_timer) = result.new_vetoes.get(&sid).copied().unwrap_or((true, true));
        let view = host.pane_status(sid).ok().flatten();
        rebalance::Pane {
            awaiting_user: view.as_ref().is_some_and(|v| v.quiescent && matches!(v.status, marspot::pane_state::PaneStatus::AwaitingUser)),
            user_here: self.focused_sid == Some(sid),
            work_in_flight,
            own_timer,
            idle_for: SystemTime::now().duration_since(meta.transcript_at).unwrap_or_default(),
            since_last,
            move_in_flight,
            host_load: (self.host_load)(),
            helper_bound: result.helpers.sits_out(sid),
        }
    }
}

/// What a pane on a short account is told while it works.
pub(super) fn heads_up_body(profile: u8, part: Part) -> String {
    let what = match part.hours {
        Some(h) if h.is_finite() && h < SHORT_H => format!("按现在的用量撑不到 {SHORT_H} 小时（约 {h:.1} 小时）"),
        _ => "额度只剩不到 1%".to_string(),
    };
    heads_up::body_for(profile, &what, true)
}

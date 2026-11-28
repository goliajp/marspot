//! The plan as log lines and as the status file, amounts in points.
//!
//! `sched.plan` carries the money of the plan over the pane horizon
//! without the value left at its end (lapse, the lapse the loads at its
//! end leave at a renewal soon after it, reserve taken at `pi`,
//! rewrites, lost background work, rewrites at refusals), the gain over
//! doing nothing (value left included), the fluid bound of the week
//! from level 1 and the difference, the switches with their windows,
//! the counts against the limits, refusals, and what today's rule
//! would do with the same panes (`today=`), so the two can be read
//! side by side.  It fits the log's line limit; the rest is in the
//! status file.

use std::fmt::Write;

use super::{Account, Input, Plan};

/// Switches written in the line; the rest are counted.
const LINE_SWITCHES: usize = 3;
const LINE_TODAY: usize = 4;
/// The status file lists at most this many accounts and panes.
const STATUS_ACCOUNTS: usize = 16;
const STATUS_PANES: usize = 64;

fn pts(w: f64) -> String {
    format!("{:.2}", w * 100.0)
}

/// What the line says besides the plan.
pub struct Said<'a> {
    pub trigger: &'a str,
    pub id: u64,
    pub prev: u64,
    /// Level 1's money over the week, weeks.
    pub bound: f64,
    /// Counted switches today and the limit, notices this hour and the limit.
    pub fleet: (usize, usize),
    pub notices: (usize, usize),
    /// Refusals in the last day the plan foresaw and did not.
    pub refusals: (usize, usize),
    /// Today's rule's would-be moves: pane, account.
    pub today: &'a [(u64, u8)],
}

impl Plan {
    /// The money over the pane horizon, without what is left at its end.
    pub fn money_h2(&self, pi: f64) -> f64 {
        let t = &self.totals;
        t.lapsed + t.lapse_after + pi * t.reserve_taken + t.rewrites + t.lost_work + t.refused
    }

    /// `sched.plan`.
    pub fn line(&self, input: &Input, s: &Said) -> String {
        let t = &self.totals;
        let money = self.money_h2(input.pi);
        let mut out = String::new();
        let _ = write!(
            out,
            "trigger={} mode={} plan={:08x} prev={:08x} money={} lapsed={} after={} reserve={} rewrites={} lost={} refused={} gain={} eff={:.2} bound={} gap={} sw=",
            s.trigger,
            self.mode.word(),
            s.id as u32,
            s.prev as u32,
            pts(money),
            pts(t.lapsed),
            pts(t.lapse_after),
            pts(input.pi * t.reserve_taken),
            pts(t.rewrites),
            pts(t.lost_work),
            pts(t.refused),
            pts(self.idle.money - t.money),
            t.eff,
            pts(s.bound),
            pts(money - s.bound),
        );
        let sw: Vec<String> = self.switches.iter().take(LINE_SWITCHES).map(|w| format!("{}:P{}>P{}@{:.1}-{:.1}/{}", w.sid, w.from, w.to, w.t_open, w.t_dead, w.kind.word())).collect();
        out.push_str(if sw.is_empty() { "-" } else { "" });
        out.push_str(&sw.join(","));
        if self.switches.len() > LINE_SWITCHES {
            let _ = write!(out, "+{}", self.switches.len() - LINE_SWITCHES);
        }
        let today: Vec<String> = s.today.iter().take(LINE_TODAY).map(|(sid, to)| format!("{sid}>P{to}")).collect();
        let agree = s.today.iter().filter(|(sid, to)| self.switches.iter().any(|w| w.sid == *sid && w.order == 0 && w.to == *to)).count();
        let _ = write!(
            out,
            " fleet={}/{} notices_h={}/{} refusals_24h={}/{} foreseen={} today={} agree={agree}/{} active={} sims={} charge={}",
            s.fleet.0,
            s.fleet.1,
            s.notices.0,
            s.notices.1,
            s.refusals.0,
            s.refusals.1,
            self.refusals.len(),
            if today.is_empty() { "-".to_string() } else { today.join(",") },
            s.today.len(),
            self.active,
            self.sims,
            pts(self.charge),
        );
        let _ = write!(out, " prof={}", input.limits.profile.word());
        out
    }

    /// `sched.plan.kept`.
    pub fn kept_line(standing: f64, candidate: f64, panes: &[u64]) -> String {
        let panes: Vec<String> = panes.iter().map(u64::to_string).collect();
        format!("standing={} candidate={} panes={}", pts(standing), pts(candidate), panes.join(","))
    }

    /// The status file: segments, accounts, pane schedules, the
    /// routing order per class and the planned claims, fixed in size.
    pub fn status(&self, input: &Input, at: i64, id: u64, reserve: &[f64], pins: &[(u64, f64)], claims: &[(u8, f64, f64)]) -> String {
        let mut o = format!("{{\"at\":{at},\"plan\":\"{id:016x}\",\"mode\":\"{}\",\"segments\":[", self.mode.word());
        o.push_str(&self.cuts.iter().map(|c| format!("{c:.2}")).collect::<Vec<_>>().join(","));
        o.push_str("],\"accounts\":[");
        let acc: Vec<String> = input.accounts.iter().enumerate().take(STATUS_ACCOUNTS).map(|(j, a)| account(a, reserve.get(j).copied().unwrap_or(0.0), self.empty.get(j).copied().unwrap_or([None; 2]))).collect();
        o.push_str(&acc.join(","));
        o.push_str("],\"panes\":[");
        let panes: Vec<String> = input.panes.iter().take(STATUS_PANES).map(|p| self.pane_json(p.sid, p.on, pins)).collect();
        o.push_str(&panes.join(","));
        let route = |c: usize| {
            let mut v: Vec<&Account> = input.accounts.iter().filter(|a| a.q.open && !a.q.stale && if c == 0 { a.q.phi() > 0.0 } else { a.q.r > 0.0 }).collect();
            v.sort_by(|x, y| y.spare.total_cmp(&x.spare).then(x.q.id.cmp(&y.q.id)));
            v.iter().take(STATUS_ACCOUNTS).map(|a| a.q.id.to_string()).collect::<Vec<_>>().join(",")
        };
        let _ = write!(o, "],\"routing\":{{\"restricted\":[{}],\"plain\":[{}]}},\"claims\":[", route(0), route(1));
        let cl: Vec<String> = claims.iter().take(STATUS_ACCOUNTS).map(|(a, h, v)| format!("{{\"account\":{a},\"at_h\":{h:.2},\"value\":{}}}", pts(*v))).collect();
        o.push_str(&cl.join(","));
        o.push_str("]}\n");
        o
    }

    fn pane_json(&self, sid: u64, on: u8, pins: &[(u64, f64)]) -> String {
        let sw: Vec<String> = self
            .switches
            .iter()
            .filter(|w| w.sid == sid)
            .map(|w| format!("{{\"to\":{},\"open_h\":{:.2},\"dead_h\":{:.2},\"kind\":\"{}\",\"gain\":{},\"gain_carried\":{}}}", w.to, w.t_open, w.t_dead, w.kind.word(), pts(w.gain), w.carried))
            .collect();
        let pin = pins.iter().find(|p| p.0 == sid).map_or_else(|| "null".into(), |p| format!("{:.2}", p.1));
        format!("{{\"sid\":{sid},\"on\":{on},\"switches\":[{}],\"pin_h\":{pin}}}", sw.join(","))
    }
}

fn account(a: &Account, reserve: f64, empty: [Option<f64>; 2]) -> String {
    let h = |e: Option<f64>| e.map_or_else(|| "null".into(), |e| format!("{e:.2}"));
    format!(
        "{{\"id\":{},\"reserve\":{},\"spare\":{},\"price\":[{:.2},{:.2}],\"empty_h\":{},\"empty_high_h\":{}}}",
        a.q.id,
        pts(reserve),
        pts(a.spare),
        a.price[0],
        a.price[1],
        h(empty[0]),
        h(empty[1])
    )
}

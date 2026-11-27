//! The planner's pane schedule over Claude's panes, in shadow: worked
//! out on every scan that has a class plan and a pane, logged and
//! recorded, never executed.
//!
//! A full search runs when `trigger` says one is due (a refusal or an
//! account closing it did not foresee at once, the other reasons at
//! most once a minute); otherwise the standing structure is re-timed.
//! A full search whose structure is adopted is logged as `sched.plan`
//! and recorded as a `plan` fact (its id, the previous id, a digest of
//! the inputs, the structure); one that loses to the standing structure
//! is logged as `sched.plan.kept`, at most once per pane an hour.  A
//! committed switch that is cancelled or missed is logged as
//! `sched.plan.ended`.  The status file is written when a full search
//! changes it.
//!
//! `today=` in `sched.plan` lists, for each pane with one, the account
//! today's rule would move it to (its standing would-move), so a plan
//! line reads next to the `would_move` facts of the same panes.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use marspot_agent::sched::calib::Calib;
use marspot_agent::sched::forecast::Forecast;
use marspot_agent::sched::level1::Level1;
use marspot_agent::sched::level2::line::Said;
use marspot_agent::sched::level2::standing::{Standing, Verdict, structure_of};
use marspot_agent::sched::level2::trigger::{Now, Triggers};
use marspot_agent::sched::level2::{FLEET_PER_DAY, Input, NOTICES_PER_HOUR, Plan, pass};

use super::allocation_adapter::Allocation;
use super::allocation_inputs::Snapshot;
use super::allocation_level1::Inputs as ClassInputs;
use super::allocation_level2_input::{Built, build, digest};
use super::allocation_view::View;
use crate::plugins::allocation::{Class, KAPPA_FABLE};

const DAY_S: i64 = 86_400;
const KEPT_EVERY_S: i64 = 3600;
const REFUSALS_CAP: usize = 64;
const KEPT_CAP: usize = 64;

pub(super) struct PanePlan {
    standing: Standing,
    triggers: Triggers,
    /// The pane speeds checked against what the accounts measure.
    calib: Calib,
    /// Refusals of the last day: when, and whether the plan foresaw it.
    refusals: VecDeque<(i64, bool)>,
    refused_seen: HashMap<u64, i64>,
    /// Panes the last plan foresaw a refusal or held a switch for.
    foreseen: Vec<u64>,
    kept_said: HashMap<u64, i64>,
    status_path: PathBuf,
    status_said: String,
    ended_last: bool,
    /// Plan passes run, full searches among them.
    pub passes: usize,
    pub full: usize,
}

impl PanePlan {
    pub(super) fn new(state_root: &Path) -> Self {
        PanePlan {
            standing: Standing::default(),
            triggers: Triggers::default(),
            calib: Calib::default(),
            refusals: VecDeque::new(),
            refused_seen: HashMap::new(),
            foreseen: Vec::new(),
            kept_said: HashMap::new(),
            status_path: state_root.join("sched").join("status.json"),
            status_said: String::new(),
            ended_last: false,
            passes: 0,
            full: 0,
        }
    }

    /// Checks the pane speeds against the accounts' readings and scales
    /// them by the factor in force; a window that moves the ratio is
    /// logged as `sched.calib`.
    fn calibrate(&mut self, b: &mut Built, now: i64, log: &mut Vec<(&'static str, String)>) {
        let quants: Vec<_> = b.accounts.iter().map(|x| x.q.clone()).collect();
        let asked: Vec<f64> = quants.iter().map(|q| b.panes.iter().filter(|p| p.on == q.id).map(|p| p.speed[0] + p.speed[1]).sum()).collect();
        let was = self.calib.ratio;
        self.calib.observe(now, &quants, &asked);
        if let Some(r) = self.calib.ratio.filter(|r| Some(*r) != was) {
            log.push(("sched.calib", format!("ratio={r:.3} factor={:.3}", self.calib.factor())));
        }
        let f = self.calib.factor();
        for p in &mut b.panes {
            p.speed = p.speed.map(|x| x * f);
            p.high = p.high.map(|x| x * f);
            p.lost_work *= f;
        }
    }

    /// One scan; log lines out.  Nothing runs without a class plan or a pane.
    pub(super) fn scan(&mut self, a: &mut Allocation, snap: &Snapshot, view: &View, forecast: Option<&Forecast>, class: Option<&(ClassInputs, Level1)>) -> Vec<(&'static str, String)> {
        let mut log = Vec::new();
        let Some(class) = class else { return log };
        if view.panes.is_empty() {
            return log;
        }
        self.passes += 1;
        let now = snap.now;
        let refused = self.note_refusals(snap);
        let mut b = build(a, snap, view, forecast, class, &self.standing);
        self.calibrate(&mut b, now, &mut log);
        let input = Input { accounts: &b.accounts, panes: &b.panes, young: b.young, window_full: KAPPA_FABLE, pi: b.pi, fleet_left: b.fleet_left, notices_left: NOTICES_PER_HOUR, stale_feed: b.stale_feed };
        let quants: Vec<_> = b.accounts.iter().map(|x| x.q.clone()).collect();
        let high: Vec<f64> = b.accounts.iter().map(|x| b.panes.iter().filter(|p| p.on == x.q.id).map(|p| p.high[0] + p.high[1]).sum()).collect();
        let mut counted: Vec<u64> = b.panes.iter().map(|p| p.sid).collect();
        counted.sort_unstable();
        let offers = offers_digest(&class.0);
        let n = Now { at: now, accounts: &quants, high: &high, counted: &counted, offers, refused, ended: self.ended_last, outside: &b.outside };
        let due = self.triggers.due(&n);
        let prev = self.standing.id;
        let done = match due {
            Some(why) => {
                self.full += 1;
                let d = digest(&b);
                let r = pass::full(&mut self.standing, &input, d, now);
                self.triggers.searched(&n, &r.plan.empty.iter().map(|e| e[1]).collect::<Vec<_>>(), r.plan.cuts.get(1).copied().unwrap_or(1.0));
                self.after_full(a, &r, &input, &b, view, class, why.word(), prev, d, now, &mut log);
                Some(r)
            }
            None => pass::retime(&mut self.standing, &input, now),
        };
        self.ended_last = false;
        if let Some(r) = done {
            self.foreseen = r.plan.refusals.iter().map(|x| x.0).chain(r.plan.switches.iter().map(|s| s.sid)).collect();
            for (sid, why) in &r.ended.cancelled {
                log.push(("sched.plan.ended", format!("sid={sid} cancelled why={}", why.replace(' ', "_"))));
            }
            for sid in &r.ended.missed {
                log.push(("sched.plan.ended", format!("sid={sid} missed")));
            }
            self.ended_last = !r.ended.cancelled.is_empty() || !r.ended.missed.is_empty();
        }
        log
    }

    #[allow(clippy::too_many_arguments)] // one full search's outputs and what its line reads
    fn after_full(&mut self, a: &mut Allocation, r: &pass::Pass, input: &Input, b: &Built, view: &View, class: &(ClassInputs, Level1), why: &str, prev: u64, d: u64, now: i64, log: &mut Vec<(&'static str, String)>) {
        match &r.verdict {
            Some(Verdict::Adopted { changed }) if *changed > 0 || prev == 0 => {
                let today = today(a, view);
                self.refusals.retain(|(at, _)| now - at < DAY_S);
                let fore = self.refusals.iter().filter(|x| x.1).count();
                let said = Said {
                    trigger: why,
                    id: self.standing.id,
                    prev,
                    bound: class.1.money.total,
                    fleet: (FLEET_PER_DAY - b.fleet_left.min(FLEET_PER_DAY), FLEET_PER_DAY),
                    notices: (0, NOTICES_PER_HOUR),
                    refusals: (fore, self.refusals.len() - fore),
                    today: &today,
                };
                log.push(("sched.plan", r.plan.line(input, &said)));
                let structure: Vec<String> = structure_of(&r.plan).iter().take(64).map(|(sid, to, o)| format!("{sid}>P{to}#{o}")).collect();
                let detail = format!("id={:016x} prev={prev:016x} digest={d:016x} trigger={why} structure={}", self.standing.id, if structure.is_empty() { "-".into() } else { structure.join(",") });
                a.event(now, 0, "plan", None, Class::Plain, detail, log);
                self.write_status(&r.plan, input, class, now, log);
            }
            Some(Verdict::Kept { standing, candidate, panes }) => {
                self.kept_said.retain(|_, at| now - *at < KEPT_EVERY_S);
                let fresh: Vec<u64> = panes.iter().copied().filter(|p| !self.kept_said.contains_key(p)).collect();
                if !fresh.is_empty() && self.kept_said.len() < KEPT_CAP {
                    for p in &fresh {
                        self.kept_said.insert(*p, now);
                    }
                    log.push(("sched.plan.kept", Plan::kept_line(*standing, *candidate, &fresh)));
                }
            }
            _ => {}
        }
    }

    /// A pane refused since the last scan, each recorded with whether
    /// the last plan foresaw it.
    fn note_refusals(&mut self, snap: &Snapshot) -> bool {
        let mut any = false;
        for p in &snap.panes {
            let Some(at) = p.refused_at else { continue };
            if self.refused_seen.get(&p.sid).is_some_and(|was| *was >= at) {
                continue;
            }
            self.refused_seen.insert(p.sid, at);
            any = true;
            if self.refusals.len() >= REFUSALS_CAP {
                self.refusals.pop_front();
            }
            self.refusals.push_back((snap.now, self.foreseen.contains(&p.sid)));
        }
        self.refused_seen.retain(|sid, _| snap.live.contains(sid));
        any
    }

    fn write_status(&mut self, plan: &Plan, input: &Input, class: &(ClassInputs, Level1), now: i64, log: &mut Vec<(&'static str, String)>) {
        let pins: Vec<(u64, f64)> = input.panes.iter().filter_map(|p| self.standing.pinned_h(p.sid, now).map(|h| (p.sid, h))).collect();
        let claims: Vec<(u8, f64, f64)> = class.1.claims.iter().map(|c| (c.account, c.at_h, c.value)).collect();
        let body = plan.status(input, 0, self.standing.id, &class.1.reserve, &pins, &claims);
        if body == self.status_said {
            return;
        }
        self.status_said = body;
        let text = plan.status(input, now, self.standing.id, &class.1.reserve, &pins, &claims);
        if let Err(e) = write_atomic(&self.status_path, &text) {
            log.push(("sched.status_failed", format!("{e}")));
        }
    }
}

/// Today's rule's standing would-move per pane: pane, account.
fn today(a: &Allocation, view: &View) -> Vec<(u64, u8)> {
    let mut v: Vec<(u64, u8)> = view.panes.iter().filter_map(|r| a.last.get(&r.0).and_then(|l| l.would).filter(|t| *t != r.1).map(|t| (r.0, t))).collect();
    v.sort_unstable();
    v
}

fn offers_digest(ci: &ClassInputs) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", ci.offers).hash(&mut h);
    h.finish()
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
#[path = "allocation_level2_tests.rs"]
mod tests;

//! The pane plan's side of the executor: what each pass hands it, the
//! refusals it counts, and the small text helpers of the plan.

use std::collections::HashMap;
use std::path::Path;

use marspot_agent::sched::calib::Closed;
use marspot_agent::sched::level2::exec::{Deadlines, Watched};
use marspot_agent::sched::level2::{FableGuard, Input, Limits, Plan};

use super::super::allocation_adapter::Allocation;
use super::super::allocation_exec::{CAP, Exec, Order, Scan, Watch};
use super::super::allocation_inputs::Snapshot;
use super::super::allocation_level1::Inputs as ClassInputs;
use super::super::allocation_more::Hold;
use super::{DAY_S, PanePlan, REFUSALS_CAP};

/// The executor, the switches it watches and the waits strict
/// protection would impose, from the last pass, the limits they were
/// planned under, and the panes refused since the last scan.
pub(super) struct ExecRun {
    pub exec: Exec,
    pub watch: Vec<Watch>,
    pub waits: Vec<u64>,
    pub deadlines: Deadlines,
    limits: Limits,
    refused_now: Vec<u64>,
}

impl ExecRun {
    pub(super) fn new(state_root: &Path) -> ExecRun {
        let exec = Exec::load(state_root, super::super::rooms::unix_now());
        ExecRun { exec, watch: Vec::new(), waits: Vec::new(), deadlines: Deadlines::default(), limits: Limits::default(), refused_now: Vec::new() }
    }

    /// The limits a pass plans under: a live executor follows a plan
    /// that protects Fable only from step E.
    pub(super) fn limits(&mut self, mut lim: Limits) -> Limits {
        if self.exec.live() && !self.exec.ladder.protects() {
            lim.fable = FableGuard::Price;
        }
        self.limits = lim;
        lim
    }
}

impl PanePlan {
    /// The executor on this scan: the switches for the main thread, and
    /// whether it acts.  A switch it called off pins its pane.
    pub(in crate::plugins::claudecode) fn execute(&mut self, a: &mut Allocation, snap: &Snapshot, holds: &HashMap<u64, Hold>, setting_live: bool, reset: Option<i64>) -> (Vec<Order>, bool, Vec<(&'static str, String)>) {
        let mut log = Vec::new();
        let now = snap.now;
        let unforeseen = self.refusals.iter().filter(|(at, foreseen)| !foreseen && now - at < DAY_S).count() as u32;
        let refused = std::mem::take(&mut self.run.refused_now);
        let r = &mut self.run;
        let scan = Scan { watches: &r.watch, lim: &r.limits, unforeseen, refused: &refused, waits: &r.waits, setting_live, reset, full: self.full };
        let orders = r.exec.step(a, snap, holds, &scan, &mut log);
        for sid in &self.run.exec.cancelled {
            self.standing.pin(*sid, now);
        }
        (orders, self.run.exec.live(), log)
    }

    /// A pane refused since the last scan, each recorded with whether
    /// the last plan foresaw it.
    pub(super) fn note_refusals(&mut self, snap: &Snapshot) -> bool {
        let mut any = false;
        for p in &snap.panes {
            let Some(at) = p.refused_at else { continue };
            if self.refused_seen.get(&p.sid).is_some_and(|was| *was >= at) {
                continue;
            }
            self.refused_seen.insert(p.sid, at);
            any = true;
            if self.run.refused_now.len() < REFUSALS_CAP {
                self.run.refused_now.push(p.sid);
            }
            if self.refusals.len() >= REFUSALS_CAP {
                self.refusals.pop_front();
            }
            self.refusals.push_back((snap.now, self.foreseen.contains(&p.sid)));
        }
        self.refused_seen.retain(|sid, _| snap.live.contains(sid));
        any
    }
}

/// The committed first switches of `plan` that the executor watches, in
/// absolute time, with what each pane's input says of it and each
/// committed one's deadline held.
pub(super) fn watches(plan: &Plan, input: &Input, now: i64, deadlines: &mut Deadlines) -> Vec<Watch> {
    let at = |h: f64| now + (h * 3600.0) as i64;
    let v = plan
        .switches
        .iter()
        .filter(|s| s.order == 0)
        .filter_map(|s| {
            let p = input.panes.iter().find(|p| p.sid == s.sid && p.on == s.from)?;
            let w = Watched { open_at: at(s.t_open), dead_at: at(s.t_dead), exempt: s.exempt, clean_per_h: p.clean_per_h, long_background: p.long_background, lost_work: p.lost_work, gain: s.gain, noticed_at: None };
            Some(Watch { sid: s.sid, from: s.from, to: s.to, w: deadlines.hold(s.sid, s.to, w, now) })
        })
        .take(CAP)
        .collect();
    deadlines.passed();
    v
}

pub(super) fn offers_digest(ci: &ClassInputs) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", ci.offers).hash(&mut h);
    h.finish()
}

pub(super) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// A closed calibration window for the log: the ratio, the factor in
/// force, and each counted account's loss / ask in points.
pub(super) fn calib_line(c: &Closed, factor: f64) -> String {
    let ratio = c.ratio.map_or("-".to_string(), |r| format!("{r:.3}"));
    let mut s = format!("ratio={ratio} factor={factor:.3} used={:.1} want={:.1}", 100.0 * c.used, 100.0 * c.want);
    for (id, used, want) in &c.accounts {
        s.push_str(&format!(" P{id}={:.1}/{:.1}", 100.0 * used, 100.0 * want));
    }
    s
}

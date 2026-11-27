//! A plan as a commitment across passes.
//!
//! The structure of a plan is which pane goes to which account, in what
//! order; the timing is the windows.  Timing is worked out again at
//! every pass with no inertia.  A new structure replaces the standing
//! one only when its money is lower by more than `max(0.05 points, 25%
//! of the switch's gain)` summed over the switches it changes, or it
//! costs no more and saves a quarter of a stall hour (the selector's own
//! rule between money and efficiency), or when the standing one no
//! longer holds (a pane gone or moved, a target that fails a filter).
//!
//! A switch whose window is open is committed: it is not re-targeted
//! for a better score, only cancelled when its target fails a filter or
//! its gain is gone: it saves neither money nor stall hours.  A window whose deadline passes with the pane where
//! it was is a miss.  After a miss or a cancel the pane is pinned to its
//! account for an hour.
//!
//! Plan ids chain: each is a hash of the previous id, a digest of the
//! inputs and the structure, so a replay from the first plan fact
//! gives the same ids.

use super::grid::Grid;
use super::{DWELL_H, Input, MIN_GAIN, MIN_GAIN_EFF, Plan, Sched};

/// A pin after a miss or a cancel, seconds.
pub const PIN_S: i64 = 3600;
/// Share of a switch's gain a change of structure must beat.
pub const INERTIA: f64 = 0.25;
/// Pins kept at most.
const PINS_CAP: usize = 64;
const SWITCHES_CAP: usize = 128;
/// Windows are kept to the second: a window opening at a cut may read
/// up to half a second after it.
const SECOND_H: f64 = 1.0 / 3600.0;
/// Float noise in a sum of cut times, hours.
const DWELL_EPS_H: f64 = 1e-9;

/// One switch of the standing plan, in absolute time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Planned {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub order: u8,
    pub open_at: i64,
    pub dead_at: i64,
    pub gain: f64,
    pub gain_eff: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Standing {
    pub id: u64,
    pub switches: Vec<Planned>,
    pins: Vec<(u64, i64)>,
}

/// What a pass did with the structure.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// The new structure was adopted; the switches it changed.
    Adopted { changed: usize },
    /// The standing one was kept: its money and the candidate's.
    Kept { standing: f64, candidate: f64, panes: Vec<u64> },
}

/// What happened to committed switches this pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ended {
    pub cancelled: Vec<(u64, &'static str)>,
    pub missed: Vec<u64>,
}

fn fnv(h: u64, x: u64) -> u64 {
    let mut h = h;
    for b in x.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The id of a plan with `structure` after `prev`, on inputs `digest`.
pub fn plan_id(prev: u64, digest: u64, structure: &[(u64, u8, u8)]) -> u64 {
    let mut h = fnv(fnv(0xcbf2_9ce4_8422_2325, prev), digest);
    for &(sid, to, order) in structure {
        h = fnv(fnv(h, sid), u64::from(to) << 8 | u64::from(order));
    }
    h
}

/// `(sid, to, order)` of every switch, sorted.
pub fn structure_of(plan: &Plan) -> Vec<(u64, u8, u8)> {
    let mut v: Vec<(u64, u8, u8)> = plan.switches.iter().map(|s| (s.sid, s.to, s.order)).collect();
    v.sort_unstable();
    v
}

impl Standing {
    /// Hours pane `sid` stays pinned from `now`.
    pub fn pinned_h(&self, sid: u64, now: i64) -> Option<f64> {
        self.pins.iter().find(|p| p.0 == sid && p.1 > now).map(|p| (p.1 - now) as f64 / 3600.0)
    }

    pub fn pin(&mut self, sid: u64, now: i64) {
        self.pins.retain(|p| p.0 != sid && p.1 > now);
        if self.pins.len() < PINS_CAP {
            self.pins.push((sid, now + PIN_S));
        }
    }

    pub fn committed(&self, now: i64) -> impl Iterator<Item = &Planned> {
        self.switches.iter().filter(move |s| s.open_at <= now)
    }

    /// The standing schedules on `input`'s panes, re-timed to `now`;
    /// `None` when a switch no longer holds.
    pub fn schedules(&self, input: &Input, g: &Grid, now: i64) -> Option<Vec<Sched>> {
        let mut out = vec![Sched::STAY; input.panes.len()];
        let mut sw = self.switches.clone();
        sw.sort_by_key(|s| (s.sid, s.order));
        for s in &sw {
            let p = input.panes.iter().position(|x| x.sid == s.sid)?;
            let j = input.accounts.iter().position(|a| a.q.id == s.to)?;
            let open_h = ((s.open_at - now) as f64 / 3600.0).max(0.0);
            let at = if s.order == 0 {
                if input.panes[p].on != s.from || !g.admits(input, p, j) {
                    return None;
                }
                g.cuts.iter().position(|c| *c >= open_h - SECOND_H).unwrap_or(0).max(g.first[p])
            } else {
                // the rounding tolerance is for the stored window only:
                // the dwell after the first switch is exact
                let dwell = g.cuts[out[p].hops().first()?.at as usize] + DWELL_H;
                g.cuts.iter().position(|c| *c >= open_h - SECOND_H && *c >= dwell - DWELL_EPS_H)?
            };
            if at >= g.segs() || out[p].n as usize != s.order as usize {
                return None;
            }
            out[p].push(j, at);
        }
        (0..out.len()).all(|p| g.in_time(p, &out[p])).then_some(out)
    }

    /// Committed switches whose gain is gone or whose deadline passed are
    /// ended; their panes are pinned.  `timed` is the standing plan
    /// worked out on this pass's inputs.
    pub fn end(&mut self, timed: Option<&Plan>, input: &Input, now: i64) -> Ended {
        let mut ended = Ended::default();
        let mut keep = Vec::with_capacity(self.switches.len());
        for s in self.switches.clone() {
            let on = input.panes.iter().find(|p| p.sid == s.sid).map(|p| p.on);
            let gain = timed.and_then(|t| t.switches.iter().find(|x| x.sid == s.sid && x.order == s.order)).map(|x| (x.gain, x.gain_eff));
            let why = if s.open_at > now || s.order > 0 {
                None
            } else if on.is_none() || on == Some(s.to) {
                Some("done or gone")
            } else if s.dead_at <= now {
                ended.missed.push(s.sid);
                Some("missed")
            } else if timed.is_none() {
                Some("target fails a filter")
            } else if gain.is_none_or(|(g, eff)| g <= 0.0 && eff <= 0.0) {
                Some("gain gone")
            } else {
                None
            };
            match why {
                Some("done or gone") => {}
                Some("missed") => self.pin(s.sid, now),
                Some(w) => {
                    ended.cancelled.push((s.sid, w));
                    self.pin(s.sid, now);
                }
                None => keep.push(s),
            }
        }
        let gone: Vec<u64> = ended.cancelled.iter().map(|c| c.0).chain(ended.missed.iter().copied()).collect();
        keep.retain(|s| !gone.contains(&s.sid));
        self.switches = keep;
        ended
    }

    /// Weighs `candidate` (a full search) against the standing plan as
    /// worked out on the same inputs, `timed`; adopts it or keeps the
    /// standing structure re-timed.
    pub fn weigh(&mut self, candidate: &Plan, timed: Option<&Plan>, digest: u64, now: i64) -> Verdict {
        let new = structure_of(candidate);
        let Some(timed) = timed else {
            return self.adopt(candidate, &new, digest, now);
        };
        let old = structure_of(timed);
        let changed: Vec<&(u64, u8, u8)> = new.iter().filter(|x| !old.contains(x)).chain(old.iter().filter(|x| !new.contains(x))).collect();
        if changed.is_empty() {
            self.retime(timed, now);
            return Verdict::Adopted { changed: 0 };
        }
        let gain_of = |x: &(u64, u8, u8)| candidate.switches.iter().chain(&timed.switches).find(|s| (s.sid, s.to, s.order) == *x).map_or(0.0, |s| s.gain);
        let bar: f64 = changed.iter().map(|x| MIN_GAIN.max(INERTIA * gain_of(x))).sum();
        let (c, t) = (&candidate.totals, &timed.totals);
        if c.money < t.money - bar || (c.money <= t.money + 1e-9 && c.eff <= t.eff - MIN_GAIN_EFF) {
            return self.adopt(candidate, &new, digest, now);
        }
        self.retime(timed, now);
        let mut panes: Vec<u64> = changed.iter().map(|x| x.0).collect();
        panes.dedup();
        Verdict::Kept { standing: timed.totals.money, candidate: candidate.totals.money, panes }
    }

    fn adopt(&mut self, plan: &Plan, structure: &[(u64, u8, u8)], digest: u64, now: i64) -> Verdict {
        let changed = structure.len().max(self.switches.len());
        self.id = plan_id(self.id, digest, structure);
        self.retime(plan, now);
        Verdict::Adopted { changed }
    }

    /// The windows of `plan`, in absolute time.
    pub fn retime(&mut self, plan: &Plan, now: i64) {
        let at = |h: f64| now + (h * 3600.0).round() as i64;
        self.switches = plan
            .switches
            .iter()
            .take(SWITCHES_CAP)
            .map(|s| Planned { sid: s.sid, from: s.from, to: s.to, order: s.order, open_at: at(s.t_open), dead_at: at(s.t_dead), gain: s.gain, gain_eff: s.gain_eff })
            .collect();
    }
}

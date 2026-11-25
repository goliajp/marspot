//! The message a pane gets, when it may get one, and how often.
//!
//! The shell sends it only with `zombie.notify = live`; in the dry run
//! it logs the message it would send (`zombie.would_tell`) under the
//! same conditions and the same pacing, so the dry run's log shows how
//! many messages would have gone out.

use std::collections::{HashMap, VecDeque};

use super::check::Suspect;
use super::rules::HOUR;

/// The same pane at most once in this long.
pub const PANE_GAP: u64 = 4 * HOUR;
pub const PANE_PER_DAY: usize = 3;
pub const HOST_PER_HOUR: usize = 3;
pub const HOST_PER_DAY: usize = 12;
/// The main session's turn has been over for this long.
pub const TURN_OVER: u64 = 120;
/// Items listed in one message; the rest are counted.
const ITEMS: usize = 12;
const BODY_BYTES: usize = 4096;
const DAY: u64 = 24 * HOUR;

/// Items remembered as told, at most.
pub const TOLD_CAP: usize = 2048;

#[derive(Default)]
pub struct Pacer {
    /// Every window is divided by this (0 reads as 1); see `limits::scale`.
    pub scale: u64,
    /// sid → when it was told, within the last day.
    panes: HashMap<u64, VecDeque<u64>>,
    host: VecDeque<u64>,
    /// Items already in a message, never in another: id → (sid, when
    /// first told).  Forgotten only when the item is gone.
    told: HashMap<String, (u64, u64)>,
}

impl Pacer {
    /// Whether a message to `sid` may go out at `now`.
    pub fn allows(&self, sid: u64, now: u64) -> bool {
        self.holds(sid, now).is_none()
    }

    /// Which limit holds a message to `sid` back at `now`, if one does:
    /// `pane_gap`, `pane_day`, `host_hour` or `host_day`.
    pub fn holds(&self, sid: u64, now: u64) -> Option<&'static str> {
        let k = self.scale.max(1);
        let (gap, hour_w, day_w) = (PANE_GAP / k, HOUR / k, DAY / k);
        let pane = self.panes.get(&sid);
        let last = pane.and_then(|q| q.back()).copied();
        let pane_day = pane.map_or(0, |q| q.iter().filter(|t| now < *t + day_w).count());
        let hour = self.host.iter().filter(|t| now < *t + hour_w).count();
        let day = self.host.iter().filter(|t| now < *t + day_w).count();
        if last.is_some_and(|t| now < t + gap) {
            Some("pane_gap")
        } else if pane_day >= PANE_PER_DAY {
            Some("pane_day")
        } else if hour >= HOST_PER_HOUR {
            Some("host_hour")
        } else if day >= HOST_PER_DAY {
            Some("host_day")
        } else {
            None
        }
    }

    pub fn record(&mut self, sid: u64, now: u64, items: impl IntoIterator<Item = String>) {
        self.clock(sid, now);
        for id in items {
            self.told.entry(id).or_insert((sid, now));
        }
        self.cap_told();
    }

    fn clock(&mut self, sid: u64, at: u64) {
        let q = self.panes.entry(sid).or_default();
        q.push_back(at);
        q.retain(|t| at < *t + DAY);
        self.host.push_back(at);
        self.host.retain(|t| at < *t + DAY);
    }

    /// Past the cap the earliest told go first.
    fn cap_told(&mut self) {
        while self.told.len() > TOLD_CAP {
            let Some(oldest) = self.told.iter().min_by_key(|(id, (_, at))| (*at, (*id).clone())).map(|(id, _)| id.clone()) else { break };
            self.told.remove(&oldest);
        }
    }

    pub fn told(&self, item: &str) -> bool {
        self.told.contains_key(item)
    }

    /// Forget the items that are gone: a process no longer running, a
    /// subagent finished.
    pub fn keep_alive(&mut self, alive: impl Fn(&str) -> bool) {
        self.told.retain(|k, _| alive(k));
    }

    /// Forget the pacing of panes that are closed.
    pub fn keep_panes(&mut self, open: impl Fn(u64) -> bool) {
        self.panes.retain(|sid, _| open(*sid));
    }
}

/// What a pane shows about whether a message may reach it now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    /// The main session's last record ends a turn.
    pub turn_ended: bool,
    /// How long the pane has been quiet and waiting for its person;
    /// None while it is not.
    pub awaiting_for: Option<u64>,
    /// The person's cursor is in the pane.
    pub user_here: bool,
    /// The pane's account refuses it.
    pub refused: bool,
    /// The pane's claude was put to rest.
    pub dormant: bool,
}

/// Whether `suspects` of one pane make a message due: one of them was
/// never told, the main session's turn ended and the pane has waited
/// for its person `TURN_OVER` (divided by `scale`), the person is
/// elsewhere, the account answers and the claude is not at rest.
/// Otherwise the first condition that fails, for the log.
pub fn due(suspects: &[&Suspect], p: &Pacer, reach: Option<&Reach>, scale: u64) -> Result<(), &'static str> {
    if !suspects.iter().any(|s| !p.told(&s.hit.key.id)) {
        return Err("told");
    }
    let Some(r) = reach else { return Err("no_pane_state") };
    if r.dormant {
        return Err("dormant");
    }
    if !r.turn_ended {
        return Err("turn_running");
    }
    if r.awaiting_for.is_none_or(|t| t < TURN_OVER / scale.max(1)) {
        return Err("not_waiting_long");
    }
    if r.user_here {
        return Err("user_here");
    }
    if r.refused {
        return Err("quota_refused");
    }
    Ok(())
}

/// The message for one pane's suspects and how many of them it lists,
/// the first ones; the rest are only counted and stay untold.
pub fn body(suspects: &[&Suspect]) -> (String, usize) {
    let mut out = format!(
        "marspot 提醒：程序检查发现这个会话名下有 {} 项疑似已经没用的工作。marspot 不会自己结束它，请你核对后处理；处理完不用回复，不处理也不会再提醒同一项。\n\n",
        suspects.len()
    );
    let mut shown = 0;
    for (i, s) in suspects.iter().enumerate() {
        let item = format!("{}. {}\n\n", i + 1, s.hit.evidence);
        if shown == ITEMS || out.len() + item.len() > BODY_BYTES - 256 {
            break;
        }
        out.push_str(&item);
        shown += 1;
    }
    if shown < suspects.len() {
        out.push_str(&format!("另有 {} 项。\n\n", suspects.len() - shown));
    }
    out.push_str("不需要的：调用 ProcStop，ids 照抄上面的 pid@数字（会连同它下面的进程一起结束），或者用 TaskStop 加任务 id。\n不确定的：先读它的输出文件再决定。");
    (out, shown)
}

mod tsv;

#[cfg(test)]
mod tests;

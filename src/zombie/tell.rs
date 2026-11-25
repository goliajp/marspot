//! The message a pane would get, and how often one may go out.
//!
//! Nothing is sent yet: the shell logs the message it would send
//! (`zombie.would_tell`).  The pacing is the real one, so the dry run's
//! log shows how many messages the limits let through.

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
        let pane = self.panes.get(&sid);
        let last = pane.and_then(|q| q.back()).copied();
        let pane_day = pane.map_or(0, |q| q.iter().filter(|t| now < *t + DAY).count());
        let hour = self.host.iter().filter(|t| now < *t + HOUR).count();
        let day = self.host.iter().filter(|t| now < *t + DAY).count();
        last.is_none_or(|t| now >= t + PANE_GAP) && pane_day < PANE_PER_DAY && hour < HOST_PER_HOUR && day < HOST_PER_DAY
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

/// Whether `suspects` of one pane make a message due: one of them was
/// never told, and the main session's turn ended `TURN_OVER` ago.
pub fn due(suspects: &[&Suspect], p: &Pacer, turn_ended: Option<u64>, now: u64) -> bool {
    suspects.iter().any(|s| !p.told(&s.hit.key.id)) && turn_ended.is_some_and(|t| now >= t + TURN_OVER)
}

/// The message for one pane's suspects.
pub fn body(suspects: &[&Suspect]) -> String {
    let mut out = format!(
        "marspot 提醒：程序检查发现这个会话名下有 {} 项疑似已经没用的工作。marspot 不会自己结束它，请你核对后处理；处理完不用回复，不处理也不会再提醒同一项。\n\n",
        suspects.len()
    );
    let mut shown = 0;
    for (i, s) in suspects.iter().enumerate() {
        let item = format!("{}. [{}] {}\n\n", i + 1, s.hit.key.rule.name(), s.hit.evidence);
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
    out
}

mod tsv;

#[cfg(test)]
mod tests;

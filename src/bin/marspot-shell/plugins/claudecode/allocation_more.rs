//! The facts a planner needs besides use and rests: what held each turn
//! end (`boundary`), compactions (`compacted`), each change of an
//! account's reading (`reading`), and the hourly roll-up.
//!
//! - `boundary`: at every turn end of a bound pane (its `rest` fact),
//!   what would hold a move there, the age of its oldest live
//!   background task and its context.  At most one per pane in
//!   `BOUNDARY_GAP`; a turn end inside that gap is written when the gap
//!   is over if the pane still rests, so every rest that long has one.
//! - `compacted`: the context before and after, from the transcript.
//! - `reading`: an account's week and Fable readings and renewals,
//!   whenever one of them changes.
//! - `expiry`: what an account's cycle left unused, when it is spent
//!   and when it renews (`allocation_expiry`).

use std::collections::HashMap;
use std::path::Path;

use super::allocation_adapter::Allocation;
use super::allocation_events::{DAY, Event};
use super::allocation_hours::Hours;
use super::allocation_inputs::{AccountIn, Snapshot};
use super::allocation_turns::Turns;

#[path = "allocation_expiry.rs"]
mod expiry;

/// At most one `boundary` row per pane in this many seconds.
pub(super) const BOUNDARY_GAP: i64 = 30;
/// A person who wrote to the pane this recently is still at it.
pub(super) const USER_S: i64 = 600;
/// A pane in focus, or written to this recently, is attended, seconds.
pub(super) const ATTENDED_S: i64 = 3600;

/// Someone is with the pane: it has focus or was written to within
/// `ATTENDED_S`.  The planner and the executor both read this.  The last
/// prompt comes from the transcript, first read from its tail when the
/// shell starts, so a restart keeps a pane written to before it attended.
pub(super) fn attended(focus: bool, prompt_at: Option<i64>, now: i64) -> bool {
    focus || prompt_at.is_some_and(|at| now - at < ATTENDED_S)
}

/// What the scan knows of a pane beyond its transcript.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Hold {
    /// A process runs under claude that is not on the resting list;
    /// `None` when the process tree was not read, which holds a move as
    /// the mover itself does.
    pub child: Option<bool>,
    pub timer: Option<bool>,
    /// It is a helper, or helpers work for it.
    pub helpers: bool,
    pub focus: bool,
    pub claude_pid: i32,
}

#[derive(Debug, Default)]
struct PaneTurn {
    resting: bool,
    last_row: Option<i64>,
    /// A turn end with no row yet.
    due: bool,
}

#[derive(Debug)]
pub(super) struct More {
    turns: Turns,
    panes: HashMap<u64, PaneTurn>,
    /// Per account, the reading last written; `None` before the files are read.
    readings: Option<HashMap<u8, String>>,
    pub(super) hours: Hours,
    /// The roll-up failed and said so; quiet until it works again.
    hours_failing: bool,
    expiry: expiry::Expiry,
}

impl More {
    pub(super) fn new(hours: Hours) -> Self {
        More { turns: Turns::default(), panes: HashMap::new(), readings: None, hours, hours_failing: false, expiry: Default::default() }
    }
}

impl Allocation {
    /// One scan's turn ends, compactions, readings and hours.
    pub(super) fn record_more(&mut self, snap: &Snapshot, holds: &HashMap<u64, Hold>) -> Vec<(&'static str, String)> {
        let mut log = Vec::new();
        let now = snap.now;
        for p in &snap.panes {
            let hold = holds.get(&p.sid).copied().unwrap_or_default();
            let Some(t) = p.transcript.as_deref() else { continue };
            for c in self.more.turns.read(t, hold.claude_pid) {
                let detail = format!("before={} after={} trigger={} at={}", c.before, c.after, if c.auto { "auto" } else { "manual" }, c.at);
                self.event(now, p.sid, "compacted", Some(p.profile), p.class, detail, &mut log);
            }
            let pt = self.more.panes.entry(p.sid).or_default();
            if p.resting && !pt.resting {
                pt.due = true;
            }
            pt.resting = p.resting;
            let gap_over = pt.last_row.is_none_or(|at| now - at >= BOUNDARY_GAP);
            if !(pt.due && p.resting && gap_over) {
                continue;
            }
            pt.due = false;
            pt.last_row = Some(now);
            let detail = boundary_detail(&self.more.turns, t, hold, self.context_of(p), now);
            self.event(now, p.sid, "boundary", Some(p.profile), p.class, detail, &mut log);
        }
        // without the usage feed no pane is listed, and dropping every
        // watch would read each transcript again and lose the compactions
        // in between
        if snap.feed_at.is_some() {
            let live: std::collections::HashSet<&Path> = snap.panes.iter().filter_map(|p| p.transcript.as_deref()).collect();
            self.more.turns.retain(|t| live.contains(t));
        }
        self.more.panes.retain(|sid, _| snap.live.contains(sid));
        self.readings(&snap.accounts, now, &mut log);
        match self.more.hours.tick(&self.events, now) {
            Err(e) if !self.more.hours_failing => {
                self.more.hours_failing = true;
                log.push(("alloc.hours_failed", format!("{e}")));
            }
            Err(_) => {}
            Ok(_) => self.more.hours_failing = false,
        }
        log
    }

    /// What runs in the background of a pane's transcript, and when a
    /// person last wrote to it.
    pub(super) fn open_of(&self, transcript: &Path, child: bool, now: i64) -> super::allocation_turns::Open {
        self.more.turns.open(transcript, child, now)
    }

    fn readings(&mut self, accounts: &[AccountIn], now: i64, log: &mut Vec<(&'static str, String)>) {
        if self.more.readings.is_none() {
            let mut last = HashMap::new();
            for e in self.events.read_since(now - DAY, now).into_iter().filter(|e| e.kind == "reading") {
                if let Some(p) = e.profile {
                    last.insert(p, reading_key(&e.detail).to_string());
                }
            }
            self.more.readings = Some(last);
        }
        let profiles: Vec<u8> = accounts.iter().map(|a| a.profile).collect();
        self.more.expiry.retain(&profiles);
        for a in accounts {
            if let Some(detail) = self.more.expiry.note(a) {
                let ev = Event { at: now, sid: 0, kind: "expiry".into(), profile: Some(a.profile), class: "plain".into(), detail };
                if let Err(e) = self.events.append(&ev) {
                    log.push(("alloc.events_failed", format!("{e}")));
                }
            }
            let detail = reading_detail(a);
            let key = reading_key(&detail).to_string();
            if self.more.readings.as_ref().and_then(|r| r.get(&a.profile)) == Some(&key) {
                continue;
            }
            let ev = Event { at: now, sid: 0, kind: "reading".into(), profile: Some(a.profile), class: "plain".into(), detail };
            if let Err(e) = self.events.append(&ev) {
                log.push(("alloc.events_failed", format!("{e}")));
            }
            self.more.readings.get_or_insert_with(HashMap::new).insert(a.profile, key);
        }
    }
}

/// `holds=` names what would hold a move at this turn end, `-` when
/// nothing does; then each part on its own.
fn boundary_detail(turns: &Turns, transcript: &Path, h: Hold, tokens: Option<u64>, now: i64) -> String {
    // an unread process tree holds, as it does for the mover
    let (child, timer) = (h.child.unwrap_or(true), h.timer.unwrap_or(true));
    let open = turns.open(transcript, child, now);
    let user = open.prompt_at.is_some_and(|at| now - at < USER_S);
    let parts = [("tasks", open.tasks > 0), ("helpers", h.helpers), ("child", child), ("timer", timer), ("user", user), ("focus", h.focus)];
    let holds: Vec<&str> = parts.iter().filter(|(_, on)| *on).map(|(k, _)| *k).collect();
    let b = |on: bool| u8::from(on);
    let known = |v: Option<bool>| v.map_or_else(|| "-".to_string(), |on| b(on).to_string());
    format!(
        "holds={} tasks={} oldest_s={} helpers={} child={} timer={} user={} focus={} tokens={}",
        if holds.is_empty() { "-".to_string() } else { holds.join(",") },
        open.tasks,
        open.oldest_s.map_or_else(|| "-".to_string(), |s| s.to_string()),
        b(h.helpers),
        known(h.child),
        known(h.timer),
        b(user),
        b(h.focus),
        tokens.map_or_else(|| "-".to_string(), |t| t.to_string()),
    )
}

/// The reading as the feed gives it, and when it was taken.
fn reading_detail(a: &AccountIn) -> String {
    let fable = a.fable_used.map_or_else(|| "-".to_string(), |f| format!("{f}"));
    format!("week={} fable={fable} week_renews={} fable_renews={} open={} measured_at={}", a.week_used, a.week_renews, a.fable_renews, u8::from(a.open), a.measured_at)
}

/// A reading without the time it was taken: a poll that reads the same
/// is no change.
fn reading_key(detail: &str) -> &str {
    detail.split(" measured_at=").next().unwrap_or(detail)
}

#[cfg(test)]
#[path = "allocation_more_tests.rs"]
mod tests;

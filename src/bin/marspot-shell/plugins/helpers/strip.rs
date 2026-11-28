//! What the strip under a pane lists: one record for each pane with
//! helpers or other shadow panes below it, at whatever depth, for the
//! core to draw.  The title's `+N` is counted from the same records.
//!
//! Composed on the main loop when what it is made of changed, and sent
//! only when a record differs from what the core has.  How long a
//! helper has run is the core's to work out.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use marspot::shell_proto::{HELPER_STRIP_ROWS, HelperStrip, StripAgent, StripRow};

use super::asking::Asked;
use super::pending::Undelivered;
use super::table::{Event, Helper, Table};
use super::turn::{Turn, turn_reported};
use super::work::Job;
use super::{SHARED, Shared, State, caller_kind, records};
use crate::observe::Observation;
use crate::pane_decls::PaneDecls;
use crate::plugins::claudecode::with_helpers::turns::{self, Look};

pub(super) mod batch;
pub(super) mod clock;
mod plain;
mod rows;
pub(in crate::plugins) mod used;
#[cfg(test)]
#[path = "strip/helper_strip_tests.rs"]
mod helper_strip_tests;
#[cfg(test)]
#[path = "strip/helper_strip_restart_tests.rs"]
mod helper_strip_restart_tests;
#[cfg(test)]
#[path = "strip/helper_strip_row_tests.rs"]
mod helper_strip_row_tests;
#[cfg(test)]
#[path = "strip/helper_strip_idle_tests.rs"]
mod helper_strip_idle_tests;

use plain::Panes;
use rows::Live;

/// What the records were last made from, and what is remembered of the
/// helpers listed.
#[derive(Default)]
struct Memory {
    from: Option<(Arc<Table>, u64, Option<Arc<Observation>>)>,
    /// The code of each listed helper's pane, asked once while it is open.
    numbers: HashMap<u64, Option<u16>>,
    /// Those composed since the table was taken from the log, and when
    /// that was.
    looked: HashSet<u64>,
    began_ms: u64,
    /// Panes with a record that is not empty.
    roots: HashSet<u64>,
}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// A fact about a helper seen here, and when (ms after the epoch).
type Fact = (u64, u64, Event);

impl Shared {
    /// Something a strip shows changed that is not in the table.
    pub(super) fn strip_changed(&self) {
        self.strip_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// The pane at the top of the chain `parent` is in.
    pub(super) fn root_over(&self, parent: u64) -> u64 {
        let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
        let st = self.lock();
        st.batches.top(&st.table, parent, &|sid| above(&obs, sid))
    }

    /// What is over at `now_ms` is listed no more.
    pub(super) fn expire_strips(&self, now_ms: u64) {
        let st = &mut *self.lock();
        if st.batches.expire(&st.table, now_ms) {
            self.strip_changed();
        }
    }

    /// Where the turns are after a tick.
    pub(super) fn publish_turns(&self, turns: &HashMap<u64, Turn>) {
        let mut st = self.lock();
        if st.turns != *turns {
            st.turns.clone_from(turns);
            drop(st);
            self.strip_changed();
        }
    }
}

/// Whose shadow a pane `obs` shows is.
pub(super) fn above(obs: &Observation, sid: u64) -> Option<u64> {
    obs.session(sid).filter(|x| x.live).map(|x| x.entry.shadow_of)
}

/// Each running helper and its parent pane, as `(helper, parent)`.
/// Asked before the plugin started, it reads the log first: which panes
/// are helpers does not depend on which plugin starts first.
pub(crate) fn parents() -> Vec<(u64, u64)> {
    let s = super::shared();
    if s.lock().began_ms == 0 {
        super::rebuild::reload(s, &|| crate::observe::shared().fresh(crate::observe::INTERVAL));
    }
    s.table().map.iter().map(|(sid, h)| (*sid, h.started.parent)).collect()
}

/// True once everything there is to say was declared.  `host` has the
/// state of each pane.
pub(crate) fn declare(decls: &mut PaneDecls<HelperStrip>, host: &dyn crate::plugins::PluginHost) -> bool {
    let Some(s) = SHARED.get() else { return false };
    let mut memory = MEMORY.lock().unwrap_or_else(|p| p.into_inner());
    let m = memory.get_or_insert_default();
    let status = |sid| host.pane_status(sid).ok().flatten().map(|v| v.status);
    let panes = Panes { status: &status, title: &super::titles::plugin_title };
    declare_in(decls, s, m, &|| crate::observe::shared().latest(), &|sid| marspot_term::pane_code::code_for(sid).ok(), &panes);
    true
}

/// With no helpers nothing is composed until a shadow pane is open.
fn declare_in(decls: &mut PaneDecls<HelperStrip>, s: &Shared, m: &mut Memory, obs: &dyn Fn() -> Arc<Observation>, number: &dyn Fn(u64) -> Option<u16>, panes: &Panes) {
    let generation = s.strip_gen.load(Ordering::Acquire);
    let table = s.table();
    let same = m.from.as_ref().is_some_and(|(t, g, _)| Arc::ptr_eq(t, &table) && *g == generation);
    let none = table.map.is_empty() && m.roots.is_empty();
    let obs = obs();
    if none && (same || s.lock().batches.open().is_empty()) && !plain::any(&obs) {
        m.from = Some((table, generation, None));
        return;
    }
    if same && m.from.as_ref().is_some_and(|(_, _, o)| o.as_ref().is_some_and(|o| Arc::ptr_eq(o, &obs))) {
        return;
    }
    let others = plain::sids(&s.lock(), &obs);
    ask_numbers(m, &table, &obs, number, &others);
    let (strips, facts) = compose(&mut s.lock(), &obs, m, records::now_ms(), panes);
    // a write to the log is the worker's
    for (sid, at_ms, ev) in facts {
        let word = ev.word();
        if !s.queue(Job::Fact { sid, at_ms, ev: Box::new(ev) }) {
            marspot::lx_warn!("helpers.queue_full", &format!("sid={sid}: {word} not queued"));
        }
    }
    let roots: HashSet<u64> = strips.iter().map(|x| x.root_sid).collect();
    for root in m.roots.difference(&roots).filter(|r| obs.session(**r).is_some()) {
        decls.declare(*root, HelperStrip::empty(*root));
    }
    for x in &strips {
        decls.declare_ref(x.root_sid, x);
    }
    m.roots = roots;
    let idle = table.map.is_empty() && m.roots.is_empty();
    m.from = Some((table, generation, (!idle).then_some(obs)));
}

/// Ask the codes of panes that are open: one is made for a pane that
/// has none, and the file may be locked for a moment.
fn ask_numbers(m: &mut Memory, table: &Table, obs: &Observation, number: &dyn Fn(u64) -> Option<u16>, others: &[u64]) {
    let helpers = table.map.values().filter(|h| batch::listed(h)).map(|h| h.sid);
    for sid in helpers.chain(others.iter().copied()) {
        if !m.numbers.contains_key(&sid) && obs.session(sid).is_some_and(|x| x.live) {
            m.numbers.insert(sid, number(sid));
        }
    }
}

/// Bring the clocks and the batches to what is seen now; what changed.
fn bring_up(st: &mut State, obs: &Observation, m: &Memory, now_ms: u64) -> Vec<Fact> {
    let above = |sid| above(obs, sid);
    // what is over takes in nobody who goes back to work now
    st.batches.expire(&st.table, now_ms);
    let moved = st.batches.settle(&mut st.table, now_ms, &above);
    let mut facts: Vec<Fact> = moved.into_iter().map(|(sid, root)| (sid, now_ms, Event::Rooted(root))).collect();
    for h in st.table.map.values_mut().filter(|h| batch::listed(h)) {
        let sid = h.sid;
        let live = live_of(&st.turns, &st.turned, &st.at_work, &st.undelivered, &st.asking, sid);
        let works = rows::works(rows::state(h, live));
        if works && !h.clock.runs() {
            h.clock.begin(now_ms);
            // a wall clock set back leaves it as it was, which is no fact
            if h.clock.runs() {
                facts.push((sid, now_ms, Event::TurnBegan));
            }
        } else if !works && let Some(since) = h.clock.since_ms {
            let at = ended_at(h, since, !m.looked.contains(&sid), st.began_ms).unwrap_or(now_ms);
            h.clock.end(at);
            if !h.clock.runs() {
                facts.push((sid, at, Event::TurnEnded));
            }
        }
    }
    let sids: Vec<u64> = st.table.map.keys().copied().collect();
    for sid in sids {
        st.batches.join(&st.table, sid, now_ms, &above);
    }
    st.batches.expire(&st.table, now_ms);
    facts
}

/// When a turn that began at `since` ended, if that was while no shell
/// ran: when it was told, or by claude's own record.
fn ended_at(h: &Helper, since: u64, first_look: bool, began_ms: u64) -> Option<u64> {
    let told = h.notices.last().copied().filter(|t| first_look && h.noticed && *t >= since);
    let recorded = || match turn_reported(h).then(|| turns::look(h.sid))?? {
        Look::Over { at, .. } => Some(at).filter(|t| (since..began_ms).contains(t)),
        Look::NotOver | Look::Asking { .. } | Look::Running { .. } => None,
    };
    told.or_else(recorded)
}

fn live_of<'a>(turns: &HashMap<u64, Turn>, turned: &HashSet<u64>, at_work: &HashSet<u64>, undelivered: &'a Undelivered, asking: &HashMap<u64, Asked>, sid: u64) -> Live<'a> {
    let question = asking.get(&sid).is_some_and(Asked::question);
    Live { turn: turns.get(&sid).copied(), turned: turned.contains(&sid), at_work: at_work.contains(&sid), undelivered: undelivered.get(sid), question }
}

/// The record of every pane with something to list, and the facts found
/// on the way.
fn compose(st: &mut State, obs: &Observation, m: &mut Memory, now_ms: u64, panes: &Panes) -> (Vec<HelperStrip>, Vec<Fact>) {
    if m.began_ms != st.began_ms {
        m.began_ms = st.began_ms;
        m.looked.clear();
    }
    let facts = bring_up(st, obs, m, now_ms);
    let listed: HashSet<u64> = st.table.map.keys().copied().chain(st.batches.ended().iter().map(|e| e.helper.sid)).collect();
    let others = plain::sids(st, obs);
    m.numbers.retain(|sid, _| listed.contains(sid) || others.contains(sid));
    st.used.retain(|sid, _| listed.contains(sid));
    m.looked.clone_from(&listed);
    let of = |sid: u64| (m.numbers.get(&sid).copied().flatten(), st.used.get(&sid).map_or_else(used::Used::default, |k| k.used));
    let mut out = Vec::new();
    for (root, batch) in st.batches.open().iter().filter(|(root, _)| obs.session(**root).is_some()) {
        let mut rows = Vec::new();
        for h in batch.open.iter().filter_map(|sid| st.table.map.get(sid)) {
            let state = rows::state(h, live_of(&st.turns, &st.turned, &st.at_work, &st.undelivered, &st.asking, h.sid));
            let (number, used) = of(h.sid);
            rows.push(rows::row(h, state, number, used));
        }
        for e in st.batches.ended().iter().filter(|e| e.root == *root) {
            let (number, used) = of(e.helper.sid);
            rows.push(rows::ended_row(e, number, used));
        }
        let parent_agent = agent_at(obs, *root);
        out.push(HelperStrip { batch: batch.number, parent_agent, rows, ..HelperStrip::empty(*root) });
    }
    plain::add(st, obs, m, panes, &mut out);
    for x in &mut out {
        x.rows = in_order(std::mem::take(&mut x.rows));
    }
    out.sort_unstable_by_key(|x| x.root_sid);
    (out, facts)
}

/// In the order they started, without those that stopped longest ago
/// when there are more than fit.
fn in_order(mut rows: Vec<StripRow>) -> Vec<StripRow> {
    while rows.len() > HELPER_STRIP_ROWS
        && let Some((i, _)) = rows.iter().enumerate().filter(|(_, r)| r.gone).min_by_key(|(_, r)| r.ended_ms)
    {
        rows.remove(i);
    }
    rows.sort_unstable_by_key(|r| (r.started_ms, r.sid));
    rows.truncate(HELPER_STRIP_ROWS);
    rows
}

/// The agent in front in pane `sid`.
fn agent_at(obs: &Observation, sid: u64) -> StripAgent {
    match obs.session(sid).and_then(|x| x.front()).and_then(caller_kind::agent_of) {
        Some("claude") => StripAgent::Claude,
        Some("codex") => StripAgent::Codex,
        Some("gemini") => StripAgent::Gemini,
        Some("kimi") => StripAgent::Kimi,
        _ => StripAgent::Other,
    }
}

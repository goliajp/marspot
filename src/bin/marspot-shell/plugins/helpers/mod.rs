//! Helpers: an agent started by another session, in a shadow pane under
//! that session's pane, on a task; when it stops, the parent session
//! gets one message with what it wrote.
//!
//! What happened is kept as facts in `helpers/events.log`; the table in
//! memory is derived from them and rebuilt from them (and from which
//! shadow sessions are still running) when the shell starts.  The
//! requests come on the control socket's threads (`service`); waiting
//! for a helper's screen and telling the parent happen on threads of
//! their own (`work`); the plugin's tick only looks at the per-pane
//! state the shell samples anyway, at budgets and at parents with no
//! agent, and queues what is due.
//!
//! Once the app is put away and the shell stays resident, nothing here
//! runs: the plugin is stopped, which parks the worker and drops the
//! result watchers (the observer is gone then, and with it every name
//! a notice is addressed by).  Helpers keep running in their panes; the
//! next start of the plugin (in this process or the next) starts a new
//! worker, rebuilds the table and acts on what came due.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use super::{LogLevel, Plugin, PluginError, PluginHost, PluginMetadata, PLUGIN_API_VERSION, TickDue};

mod asking;
mod caller_kind;
mod close;
mod container;
mod dialogs;
mod dirs;
mod first_screens;
mod give_up;
mod inbox_of;
mod launch;
mod names;
mod notify;
mod pair;
mod passing;
mod pending;
mod queue;
mod rebuild;
mod records;
mod results;
mod screen;
mod service;
mod settle;
mod started;
mod stop_all;
pub(crate) mod strip;
mod table;
mod tell;
mod tick;
pub(crate) mod titles;
mod timers;
mod turn;
mod work;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_review;
#[cfg(test)]
mod tests_retry;
#[cfg(test)]
mod tests_notice;
#[cfg(test)]
mod tests_restart;
#[cfg(test)]
mod tests_away;
#[cfg(test)]
mod tests_trust;
#[cfg(test)]
mod tests_passing;
#[cfg(test)]
mod tests_smoke;
#[cfg(test)]
mod tests_repeat;
#[cfg(test)]
mod tests_keys;
#[cfg(test)]
mod tests_ends;
#[cfg(test)]
mod tests_again;
#[cfg(test)]
mod tests_follow;

pub(in crate::plugins) use strip::used;
pub(in crate::plugins) use pair::{Moved, is_pair, leaving, moved};
pub(in crate::plugins) use names::record_written;
use table::{Event, Table};

/// A parent pane with no agent this long loses the helpers an agent
/// started under it.
const NO_AGENT_FOR: Duration = Duration::from_secs(10 * 60);

/// What the socket's threads, the worker and the plugin share.
pub(super) struct Shared {
    state: Mutex<State>,
    /// Held while a fact is written and applied, so the log's order is
    /// the table's; the state lock is taken only for the apply.
    writing: Mutex<()>,
    /// The table as last changed, for `helper.list`.
    published: RwLock<Arc<Table>>,
    /// The queue of the worker of the latest start.
    jobs: Mutex<Option<SyncSender<work::Job>>>,
    /// A `Wake` sits in the queue: one is enough.
    wake_queued: AtomicBool,
    /// The app was put away: nothing more is done until the next start.
    parked: AtomicBool,
    /// Moved on by each park and each start: the threads of an earlier
    /// start end.
    epoch: AtomicU64,
    /// Moved on by what a strip shows that is not in the table.
    strip_gen: AtomicU64,
    /// A look at the codex contexts sits in the queue.
    contexts_queued: AtomicBool,
    /// Locked by the worker alone.
    contexts: Mutex<super::codex::context::Contexts>,
    log: PathBuf,
}

#[derive(Default)]
struct State {
    table: Table,
    /// One watcher of each helper's result file.
    watchers: HashMap<u64, marspot_term::fs_watch::Watcher>,
    /// Helpers being closed: no second close is queued, and their
    /// budgets are not waited on.
    closing: HashSet<u64>,
    /// Closes that failed with the pane still there: when to try again
    /// (ms after the epoch) and how many tries went.
    close_retry: HashMap<u64, (u64, u32)>,
    /// Parent panes seen with no agent, since when (ms after the epoch).
    no_agent: HashMap<u64, u64>,
    /// The last turn's end that was told of each claude helper
    /// (`Shared::turn_told`).
    turn_told: HashMap<u64, turn::Told>,
    /// When the last helper of each helper went (ms after the epoch).
    child_gone: HashMap<u64, u64>,
    /// Notices that could not be delivered, to try again on an event.
    undelivered: pending::Undelivered,
    /// Claude and kimi helpers given something after a stop that was
    /// told, when (ms after the epoch): no turn of theirs was seen since.
    poked: HashMap<u64, u64>,
    /// Helpers that stopped, while their session or directory may be
    /// there (`dirs`).
    stopped: HashSet<u64>,
    /// What stopped and is still listed in a strip.
    batches: strip::batch::Batches,
    /// Where each turn is, as of the last tick.
    turns: HashMap<u64, turn::Turn>,
    /// Claude and kimi helpers whose plugin has reported a turn.
    turned: HashSet<u64>,
    /// Those of them whose plugin reports one now.
    at_work: HashSet<u64>,
    /// Helpers at a prompt whose screen could not be read: when it is
    /// read again (ms after the epoch).
    prompt_unread: HashMap<u64, u64>,
    /// Claude helpers at a tool call no result follows: what their
    /// screen showed (`Shared::at_prompt`).
    asking: HashMap<u64, asking::Asked>,
    /// Claude helpers whose tool call open when last told of a prompt
    /// has ended since: a prompt like it now is another.
    answered: HashSet<u64>,
    /// Parent panes handed to another agent, not yet written down.
    moves: pair::Moves,
    /// Parent panes whose helpers wait for the name of a conversation.
    names: names::Names,
    /// What the records of the helpers' agents say of their context.
    used: HashMap<u64, strip::used::Known>,
    /// When the table was taken from the log (ms after the epoch).
    began_ms: u64,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> &'static Arc<Shared> {
    SHARED.get_or_init(|| Arc::new(Shared::at(records::dir().join("events.log"))))
}

/// Input reached pane `sid` by a message into its inbox.
pub(in crate::plugins) fn input_seen(sid: u64) {
    if let Some(s) = SHARED.get() {
        s.input_seen(sid);
    }
}

impl Shared {
    fn at(log: PathBuf) -> Shared {
        Shared {
            state: Mutex::new(State::default()),
            writing: Mutex::new(()),
            published: RwLock::new(Arc::new(Table::default())),
            jobs: Mutex::new(None),
            wake_queued: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            strip_gen: AtomicU64::new(0),
            contexts_queued: AtomicBool::new(false),
            contexts: Mutex::default(),
            log,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Write the fact down, then let it change the table.  The file is
    /// written outside the state lock, which the main loop's tick takes.
    fn record(&self, sid: u64, ev: Event) {
        self.record_at(sid, records::now_ms(), ev);
    }

    /// [`Shared::record`] of what happened at `now`.
    fn record_at(&self, sid: u64, now: u64, ev: Event) {
        self.write(sid, now, ev, true);
    }

    /// Write down what the main loop saw at `at_ms` and changed the table
    /// by then.
    fn record_seen(&self, sid: u64, at_ms: u64, ev: Event) {
        self.write(sid, at_ms, ev, false);
    }

    fn write(&self, sid: u64, now: u64, ev: Event, apply: bool) {
        let _w = self.writing.lock().unwrap_or_else(|p| p.into_inner());
        records::append(&self.log, records::ROTATE_AT, &self.table(), sid, now, &ev);
        // where a pane that stopped was
        let obs = matches!(ev, Event::Stopped { .. }).then(|| crate::observe::shared().latest());
        let mut guard = self.lock();
        let st = &mut *guard;
        let parent = st.table.map.get(&sid).map(|h| h.started.parent);
        if st.table.map.get(&sid).is_some_and(|h| turn::pokes(h, &ev)) {
            st.poked.insert(sid, now);
        }
        if matches!(ev, Event::BlockedNoticed { .. }) {
            st.answered.remove(&sid);
        }
        let contained = st.table.map.get(&sid).is_some_and(|h| h.contained());
        if apply {
            st.batches.apply(&mut st.table, sid, now, &ev, &|sid| strip::above(obs.as_deref()?, sid));
        }
        if matches!(ev, Event::Stopped { .. }) {
            st.stopped.insert(sid);
            st.watchers.remove(&sid);
            st.turned.remove(&sid);
            st.at_work.remove(&sid);
            st.poked.remove(&sid);
            st.prompt_unread.remove(&sid);
            st.asking.remove(&sid);
            st.answered.remove(&sid);
            st.closing.remove(&sid);
            st.close_retry.remove(&sid);
            st.turn_told.remove(&sid);
            st.child_gone.remove(&sid);
            if let Some(p) = parent.filter(|p| st.table.map.contains_key(p)) {
                st.child_gone.insert(p, now);
            }
        }
        *self.published.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(st.table.clone());
        drop(guard);
        if contained && matches!(ev, Event::Stopped { .. }) {
            container::remove_later(sid, self.root());
        }
        // a new deadline, or a notice held back: the worker's sleep changes
        if matches!(ev, Event::TaskDelivered | Event::NoticeSuppressed { .. }) {
            self.wake();
        }
        // when the plugin next wants a tick may have changed
        if let Some(h) = crate::pane_control::hub() {
            h.knock();
        }
    }

    fn table(&self) -> Arc<Table> {
        Arc::clone(&self.published.read().unwrap_or_else(|p| p.into_inner()))
    }

    fn input_seen(&self, sid: u64) {
        if self.table().map.get(&sid).is_some_and(|h| h.noticed) {
            self.record(sid, Event::InputSeen);
        }
    }

    /// The app was put away: stop the worker and the watchers.
    fn park(&self) {
        let mut st = self.lock();
        self.parked.store(true, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        st.watchers.clear();
        st.names.clear();
        drop(st);
        self.wake();
    }
}

pub struct HelpersPlugin {
    /// Claude and kimi helpers: where their turn is, since their last stop.
    in_turn: HashMap<u64, turn::Turn>,
    /// Parents no tick watches, as none would close their helpers.
    parked_parents: tick::Parked,
}

impl HelpersPlugin {
    pub fn new() -> Self {
        Self { in_turn: HashMap::new(), parked_parents: tick::Parked::default() }
    }
}

impl Default for HelpersPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for HelpersPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata { name: "helpers", api_version: PLUGIN_API_VERSION, tick_interval_ms: 1000 }
    }

    fn start(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        let s = Arc::clone(shared());
        self.parked_parents = tick::Parked::default();
        let w = Arc::clone(&s);
        super::claudecode::with_helpers::on_account_changed(Arc::new(move || w.wake()));
        let n = rebuild::begin(&s, rebuild::live_shadows).map_err(|e| PluginError::Other(format!("helpers worker: {e}")))?;
        self.in_turn = turn::turns_at_start(&s.table());
        s.publish_turns(&self.in_turn);
        // after the table: a rebuild closes what it finds half started
        crate::pane_control::helpers_port::register(Box::new(service::Service(Arc::clone(&s))));
        host.log(LogLevel::Info, "start", &format!("{n} helpers carried over"));
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        tick::tick(shared(), host, &mut self.in_turn, &mut self.parked_parents);
    }

    /// Worked out each time asked: an event (a helper started, a result
    /// written, a program left a pane) changes it with no tick between.
    fn tick_due(&self) -> TickDue {
        let obs = crate::observe::shared().latest();
        let st = shared().lock();
        let no_agent = st.no_agent.keys().copied().collect();
        let over = st.batches.next_ms(&st.table);
        let (retry, closing) = (st.undelivered.waiting_on(&obs), st.closing.clone());
        // u64::MAX: the read again is queued, nothing to wait for
        let reread = st.prompt_unread.values().copied().filter(|at| *at != u64::MAX).min();
        drop(st);
        let (t, now) = (shared().table(), records::now_ms());
        let due = tick::due(&t, &no_agent, &self.parked_parents, &obs, retry, now);
        let at = |ms: Option<u64>| ms.map_or(TickDue::Event, |at| tick::at(at, now));
        let timed = tick::sooner(at(over), at(reread));
        tick::sooner(tick::sooner(due, timed), tick::gone_due(&t, &closing, &obs, now))
    }

    fn stop(&mut self, _host: &dyn PluginHost) {
        shared().park();
    }
}

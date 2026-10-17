//! One view of the processes and sessions L1 watches.
//!
//! Every consumer in L1 that asks "which panes exist, what runs in
//! them" used to answer it on its own: each walked the whole process
//! table, rebuilt a parent map per pane, re-read every `entry.toml`
//! and re-read the command line of every process it looked at.  The
//! observer does that once per interval on its own thread and
//! publishes the result as an `Arc<Observation>`; consumers read the
//! latest one, or ask for one no older than they can accept.
//!
//! An observation is up to [`INTERVAL`] old.  Anything that acts on a
//! process (signals it, types at it) must re-check the `ProcId` at the
//! moment it acts; the observation is for deciding, not for acting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId, ProcRow, ProcTable};
use marspot_term::session_registry::{RegistryCache, SessionEntry, entry_is_live_with};

/// How often a new observation is built when nobody asks for one.
pub const INTERVAL: Duration = Duration::from_secs(2);

/// An observation this young counts as the present, for a consumer
/// that acts on what it sees (an op step, a CLI request).  Short enough
/// that a process started or stopped a moment ago is seen; long enough
/// that the steps of one supervisor poll share a walk.
pub const NOW: Duration = Duration::from_millis(100);

/// Builds between two log lines with the observer's totals: about every
/// ten minutes at the idle cadence.
const STATS_EVERY: u64 = 300;

/// One process under a pane's shell.
#[derive(Clone, Debug)]
pub struct Proc {
    pub row: ProcRow,
    /// argv, read once per process image.  None when it could not be
    /// read (exited, or the kernel refused).
    pub argv: Option<Arc<[String]>>,
}

impl Proc {
    /// The basename of argv[0], up to its first space.  This is what
    /// identifies an agent: released CLIs rename their process, so
    /// `comm` is not dependable, while argv[0] keeps the name they were
    /// started as.
    pub fn name(&self) -> &str {
        let Some(first) = self.argv.as_ref().and_then(|a| a.first()) else {
            return "";
        };
        let argv0 = first.split(' ').next().unwrap_or("");
        argv0.rsplit('/').next().unwrap_or(argv0)
    }

    /// argv joined by spaces, the shape `pidtree::proc_cmdline` returns.
    pub fn cmdline(&self) -> Option<String> {
        self.argv.as_ref().map(|a| a.join(" "))
    }
}

/// One registry entry as of this observation.
#[derive(Clone, Debug)]
pub struct Session {
    pub entry: SessionEntry,
    /// The entry's L3 is running (`entry_is_live_with`).
    pub live: bool,
    /// The pane's shell.  None when the session is not live, when the
    /// entry names no shell (`shell_child_pid <= 1`: a legacy entry, or
    /// one written before the fork; walking pid 0 or 1 would be the
    /// whole host), or when the shell is not in the table.
    pub shell: Option<ProcId>,
    /// Everything under the shell, in discovery order.  Empty without
    /// a shell.
    pub descendants: Vec<Proc>,
}

impl Session {
    /// The first descendant whose [`Proc::name`] is `name`.
    pub fn find(&self, name: &str) -> Option<&Proc> {
        self.descendants.iter().find(|p| p.name() == name)
    }
}

pub struct Observation {
    /// When the table was taken.
    pub at: Instant,
    pub table: ProcTable,
    /// Every registry entry that parses, in id order, live or not.
    pub sessions: Vec<Session>,
}

impl Observation {
    /// Assemble an observation from parts: what the observer does, and
    /// what a test does with a hand-built table.
    pub fn from_parts(
        at: Instant,
        table: ProcTable,
        entries: Vec<(SessionEntry, bool)>,
        mut argv: impl FnMut(&ProcRow) -> Option<Arc<[String]>>,
    ) -> Observation {
        let sessions = entries
            .into_iter()
            .map(|(entry, live)| {
                let shell = (live && entry.shell_child_pid > 1)
                    .then(|| table.get(entry.shell_child_pid).map(ProcRow::id))
                    .flatten();
                let descendants = match shell {
                    Some(id) => table
                        .descendants(id.pid)
                        .into_iter()
                        .map(|row| Proc { argv: argv(row), row: row.clone() })
                        .collect(),
                    None => Vec::new(),
                };
                Session { entry, live, shell, descendants }
            })
            .collect();
        Observation { at, table, sessions }
    }

    pub fn session(&self, sid: u64) -> Option<&Session> {
        self.sessions.iter().find(|s| s.entry.id == sid)
    }

    pub fn live(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter().filter(|s| s.live)
    }

    /// The shell pid the registry names for `sid`, 0 when there is no
    /// such entry.  Callers read 0 as "cannot tell".
    pub fn shell_pid(&self, sid: u64) -> i32 {
        self.session(sid).map_or(0, |s| s.entry.shell_child_pid)
    }

    /// Whether `id` is still the process it was when this table was
    /// taken.
    pub fn has(&self, id: ProcId) -> bool {
        self.table.get(id.pid).is_some_and(|r| r.id() == id)
    }
}

/// argv per process image.
///
/// Keyed by `ProcId` and checked against `comm`: exec keeps the pid and
/// the start time but replaces the image, and a child read between its
/// fork and its exec still carries its parent's argv.  Pruned every
/// build to the processes in the newest table, so it is bounded by the
/// live process set.
#[derive(Default)]
pub struct ArgvCache {
    map: HashMap<ProcId, (String, Arc<[String]>)>,
    reads: u64,
}

impl ArgvCache {
    pub fn get(&mut self, row: &ProcRow) -> Option<Arc<[String]>> {
        if let Some((comm, argv)) = self.map.get(&row.id())
            && *comm == row.comm
        {
            return Some(argv.clone());
        }
        self.reads += 1;
        let argv: Arc<[String]> = pidtree::proc_argv(row.pid)?.into();
        // the pid may have been reused between the table and this read
        if pidtree::proc_id(row.pid) != Some(row.id()) {
            return None;
        }
        self.map.insert(row.id(), (row.comm.clone(), argv.clone()));
        Some(argv)
    }

    pub fn prune(&mut self, table: &ProcTable) {
        self.map.retain(|id, _| table.get(id.pid).is_some_and(|r| r.id() == *id));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub fn reads(&self) -> u64 {
        self.reads
    }
}

/// Environment values of agent processes, one read per process image
/// and key.
///
/// Owned by each consumer rather than shared: the claudecode worker
/// asks about `claude` processes and codex about `codex` ones, each on
/// its own thread, so a shared map would guard nothing but add a lock
/// to the main thread.  Absent values are kept too: a default profile
/// has no `CLAUDE_CONFIG_DIR`, and that answer does not change either.
#[derive(Default)]
pub struct EnvCache {
    map: HashMap<(ProcId, &'static str), (String, Option<String>)>,
    reads: u64,
}

impl EnvCache {
    pub fn value(&mut self, row: &ProcRow, key: &'static str) -> Option<String> {
        if let Some((comm, v)) = self.map.get(&(row.id(), key))
            && *comm == row.comm
        {
            return v.clone();
        }
        self.reads += 1;
        let v = pidtree::proc_env_value(row.pid, key);
        if pidtree::proc_id(row.pid) != Some(row.id()) {
            return None;
        }
        self.map.insert((row.id(), key), (row.comm.clone(), v.clone()));
        v
    }

    pub fn prune(&mut self, obs: &Observation) {
        self.map.retain(|(id, _), _| obs.has(*id));
    }

    #[cfg(test)]
    pub fn reads(&self) -> u64 {
        self.reads
    }
}

/// What the observer thread keeps between builds.
#[derive(Default)]
struct Builder {
    registry: RegistryCache,
    argv: ArgvCache,
    /// `sid → (L3 identity, entry written_us, live)`: the liveness rule
    /// looks up the executable path, which only needs asking again when
    /// the process or the entry changed.
    live: HashMap<u64, (ProcId, u64, bool)>,
}

impl Builder {
    fn build(&mut self, walks: &AtomicU64) -> Observation {
        let obs = self.walk(walks);
        let walks = walks.load(Ordering::Relaxed);
        if walks.is_multiple_of(STATS_EVERY) {
            marspot::lx_info!(
                "shell.observe.stats",
                "process observation totals",
                walks = walks,
                table_walks = pidtree::table_walks(),
                argv_cached = self.argv.len(),
                sessions = obs.sessions.len()
            );
        }
        obs
    }

    fn walk(&mut self, walks: &AtomicU64) -> Observation {
        let at = Instant::now();
        let table = ProcTable::take();
        walks.fetch_add(1, Ordering::Relaxed);
        let entries: Vec<SessionEntry> = self.registry.list().to_vec();
        let mut seen = Vec::with_capacity(entries.len());
        let entries: Vec<(SessionEntry, bool)> = entries
            .into_iter()
            .map(|e| {
                seen.push(e.id);
                let live = self.is_live(&e, &table);
                (e, live)
            })
            .collect();
        self.live.retain(|sid, _| seen.contains(sid));
        let argv = &mut self.argv;
        let obs = Observation::from_parts(at, table, entries, |row| argv.get(row));
        self.argv.prune(&obs.table);
        obs
    }

    fn is_live(&mut self, e: &SessionEntry, table: &ProcTable) -> bool {
        let (Some(row), Some(written)) = (table.get(e.pid), self.registry.written_us(e.id)) else {
            return false;
        };
        let id = row.id();
        if let Some(&(was, w, live)) = self.live.get(&e.id)
            && was == id
            && w == written
        {
            return live;
        }
        let live = entry_is_live_with(e, written, row.start_us);
        self.live.insert(e.id, (id, written, live));
        live
    }
}

struct State {
    latest: Arc<Observation>,
    /// Someone is waiting for an observation newer than `latest`.
    wanted: bool,
    stop: bool,
    /// The observer thread has ended: no newer observation will come.
    dead: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    walks: AtomicU64,
}

impl Shared {
    /// The state, also after a panic poisoned the lock: every field is
    /// written whole, so what a panicking holder left is still sound.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Marks the observer dead when its thread ends, by return or by
/// unwinding, and wakes everyone waiting on it.
struct Obituary<'a>(&'a Shared);

impl Drop for Obituary<'_> {
    fn drop(&mut self) {
        let mut g = self.0.state();
        g.dead = true;
        if std::thread::panicking() && !g.stop {
            marspot::lx_error!(
                "shell.observe.died",
                "the observer thread panicked; consumers keep the last observation"
            );
        }
        drop(g);
        self.0.changed.notify_all();
    }
}

pub struct Observer {
    shared: Arc<Shared>,
}

/// The process-wide observer, started on first use.
pub fn shared() -> &'static Observer {
    static OBSERVER: OnceLock<Observer> = OnceLock::new();
    OBSERVER.get_or_init(|| Observer::spawn(INTERVAL))
}

impl Observer {
    /// Build the first observation on the calling thread, so no
    /// consumer ever sees an empty one, then keep building on a thread
    /// of its own every `interval`.
    pub fn spawn(interval: Duration) -> Observer {
        let mut builder = Builder::default();
        Observer::spawn_with(interval, move |walks| builder.build(walks))
    }

    /// [`spawn`](Self::spawn) with the build step given, so a test can
    /// make it fail.
    fn spawn_with(
        interval: Duration,
        mut build: impl FnMut(&AtomicU64) -> Observation + Send + 'static,
    ) -> Observer {
        let walks = AtomicU64::new(0);
        let first = build(&walks);
        let state = State { latest: Arc::new(first), wanted: false, stop: false, dead: false };
        let shared = Arc::new(Shared { state: Mutex::new(state), changed: Condvar::new(), walks });
        let theirs = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("l1-observe".into())
            .spawn(move || {
                let _obituary = Obituary(&theirs);
                run(&theirs, build, interval)
            })
            .expect("spawn the observer thread");
        Observer { shared }
    }

    /// The newest observation, without waiting.
    pub fn latest(&self) -> Arc<Observation> {
        self.shared.state().latest.clone()
    }

    /// An observation taken no more than `max_age` ago, waiting for a
    /// new one when the latest is older.  Callers asking within the same
    /// window share one build.
    ///
    /// When the observer has died this is the last observation it
    /// made, however old: the caller must not wait for one that cannot
    /// come.  Acting on a stale view is safe, because whoever acts
    /// re-checks the `ProcId` first.
    pub fn fresh(&self, max_age: Duration) -> Arc<Observation> {
        let asked = Instant::now();
        let mut g = self.shared.state();
        loop {
            if asked.saturating_duration_since(g.latest.at) <= max_age {
                return g.latest.clone();
            }
            if g.dead {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    marspot::lx_warn!(
                        "shell.observe.stale",
                        "the observer is dead; serving its last observation"
                    )
                });
                return g.latest.clone();
            }
            g.wanted = true;
            self.shared.changed.notify_all();
            g = self.shared.changed.wait(g).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Process-table walks this observer has made.
    #[cfg(test)]
    pub fn walks(&self) -> u64 {
        self.shared.walks.load(Ordering::Relaxed)
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.shared.state().stop = true;
        self.shared.changed.notify_all();
    }
}

fn run(shared: &Shared, mut build: impl FnMut(&AtomicU64) -> Observation, interval: Duration) {
    loop {
        {
            let mut g = shared.state();
            loop {
                if g.stop {
                    return;
                }
                if g.wanted {
                    break;
                }
                let due = g.latest.at + interval;
                let now = Instant::now();
                if now >= due {
                    break;
                }
                g = shared
                    .changed
                    .wait_timeout(g, due - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            g.wanted = false;
        }
        let obs = Arc::new(build(&shared.walks));
        let old = std::mem::replace(&mut shared.state().latest, obs);
        shared.changed.notify_all();
        drop(old);
    }
}

#[cfg(test)]
pub(crate) mod tests;

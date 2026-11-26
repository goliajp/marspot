//! `force`: SIGKILL, a while after SIGTERM, to what is still the same
//! process.  The wait is kept by one thread that sleeps until the
//! earliest kill is due and with nothing queued sleeps until something
//! is, so a request never waits for it.
//!
//! The queue lives only in this process: kills still waiting when the
//! shell execs itself are dropped, each handed to its `done` as `None`
//! first (`abandon`) so the drop is recorded.  If the thread dies, the
//! next request starts another, which takes over what is queued.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marspot::pidtree::{self, ProcId};

/// How long after SIGTERM what still runs gets SIGKILL.
pub(crate) const GRACE: Duration = Duration::from_secs(5);
/// Kills waiting at most; a request past it is answered without one.
pub(crate) const MAX_PENDING: usize = 64;

/// What became of one process at its kill.
pub(crate) type Killed = Vec<(ProcId, Result<(), pidtree::SignalError>)>;
/// Told what a batch's kill did, or `None` when it was dropped unsent.
pub(crate) type Done = Box<dyn FnOnce(Option<Killed>) + Send>;

struct Pending {
    at: Instant,
    ids: Vec<ProcId>,
    done: Done,
}

#[derive(Default)]
struct Queue {
    pending: Vec<Pending>,
    started: bool,
}

struct Timer {
    q: Mutex<Queue>,
    cv: Condvar,
}

fn timer() -> &'static Timer {
    static T: OnceLock<Timer> = OnceLock::new();
    T.get_or_init(|| Timer { q: Mutex::new(Queue::default()), cv: Condvar::new() })
}

/// SIGKILL each of `ids` that is still the process it names at `at`,
/// then hand what happened to `done`.  False when too many kills are
/// waiting already; nothing is queued then.
pub(crate) fn after(at: Instant, ids: Vec<ProcId>, done: Done) -> bool {
    let t = timer();
    let mut q = t.q.lock().unwrap_or_else(|p| p.into_inner());
    if q.pending.len() >= MAX_PENDING {
        return false;
    }
    if !q.started {
        let spawned = std::thread::Builder::new().name("proc-stop-force".into()).spawn(|| run(timer()));
        if let Err(e) = spawned {
            marspot::lx_warn!("shell.proc_stop.force_thread_failed", &format!("{e}"));
            return false;
        }
        q.started = true;
    }
    q.pending.push(Pending { at, ids, done });
    t.cv.notify_all();
    true
}

/// Have the shell's exec drop the kills still waiting.  At start-up.
pub(crate) fn register() {
    crate::platform::exec_hooks::register(crate::platform::exec_hooks::ExecHook { name: "zombie", before: abandon, failed: None });
}

/// Drop every kill still waiting, telling each `done` so; for just
/// before the shell replaces its image.
pub(crate) fn abandon() {
    let pending = std::mem::take(&mut timer().q.lock().unwrap_or_else(|p| p.into_inner()).pending);
    for p in pending {
        (p.done)(None);
    }
}

/// Marks the thread gone however it ends, so the next request starts
/// another instead of queueing behind a thread that no longer runs.
struct Exit;

impl Drop for Exit {
    fn drop(&mut self) {
        timer().q.lock().unwrap_or_else(|p| p.into_inner()).started = false;
        if std::thread::panicking() {
            marspot::lx_warn!("shell.proc_stop.force_thread_panicked", "the next force request starts another");
        }
    }
}

fn run(t: &Timer) {
    let _exit = Exit;
    let mut q = t.q.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        let now = Instant::now();
        let Some(next) = q.pending.iter().map(|p| p.at).min() else {
            q = t.cv.wait(q).unwrap_or_else(|p| p.into_inner());
            continue;
        };
        if next > now {
            q = t.cv.wait_timeout(q, next - now).unwrap_or_else(|p| p.into_inner()).0;
            continue;
        }
        let (due, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut q.pending).into_iter().partition(|p| p.at <= now);
        q.pending = rest;
        drop(q);
        for p in due {
            let killed = p.ids.iter().map(|id| (*id, pidtree::signal(*id, libc::SIGKILL))).collect();
            (p.done)(Some(killed));
        }
        q = t.q.lock().unwrap_or_else(|p| p.into_inner());
    }
}

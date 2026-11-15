//! Every gemini on this machine as far as it has been seen, in a pane
//! or not, by the home and directory it runs in (`plugins::beside`).
//!
//! The newest session file in a directory is a pane's gemini's own
//! only while no other gemini has been there, under the same home,
//! since it started.  Telling a gemini takes its command line, which
//! the main thread does not read for every process on the machine: the
//! peer plugin's worker looks, and the tick asks what was found.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use marspot::pidtree::{self, ProcId, ProcTable};

use super::pane::root_from;
use crate::plugins::beside::{self, Beside, Kinds};

static SEEN: Mutex<Option<Beside>> = Mutex::new(None);

/// When the oldest gemini in a pane started, in milliseconds since the
/// epoch; 0 while there is none.
static OLDEST_PANE_MS: AtomicU64 = AtomicU64::new(0);

/// What gemini keeps sessions apart by.
pub(super) fn place(root: &Path, cwd: &str) -> String {
    format!("{}\n{cwd}", root.display())
}

/// From the tick: when the oldest of the panes' geminis started.
pub(super) fn panes_started(oldest_ms: Option<u64>) {
    OLDEST_PANE_MS.store(oldest_ms.unwrap_or(0), Ordering::Relaxed);
}

/// No gemini but `me` has been seen in `place` since `since_ms`; false
/// until a look has found `me`.
pub(super) fn alone(me: ProcId, place: &str, since_ms: u64) -> bool {
    SEEN.lock().unwrap().as_ref().is_some_and(|s| s.alone(me, place, since_ms))
}

/// A look at the process table, off the main thread.
pub(crate) fn look(table: &ProcTable, kinds: &mut Kinds, user_home: &Path) {
    let geminis = kinds.of(table, super::looks_like_gemini);
    let alive = geminis
        .iter()
        // gemini relaunches itself as a child of the same name: one agent
        .filter(|r| !geminis.iter().any(|p| p.pid == r.ppid))
        .map(|r| {
            let root = root_from(pidtree::proc_env_value(r.pid, "GEMINI_CLI_HOME"), user_home);
            (r.id(), pidtree::proc_cwd(r.pid).map(|c| place(&root, &c.to_string_lossy())))
        })
        .collect();
    let oldest = Some(OLDEST_PANE_MS.load(Ordering::Relaxed)).filter(|t| *t > 0);
    SEEN.lock().unwrap().get_or_insert_with(Beside::default).note(alive, beside::now_ms(), oldest);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gemini that is in no pane counts, and one under another home
    /// keeps its own sessions.
    #[test]
    fn a_gemini_is_alone_only_while_no_other_shares_its_home_and_directory() {
        let me = ProcId { pid: 10, start_us: 60_000_000 };
        let other = ProcId { pid: 11, start_us: 61_000_000 };
        let (home, elsewhere) = (Path::new("/h"), Path::new("/h/.gemini-profile-2"));
        let mut seen = Beside::default();
        seen.note(vec![(me, Some(place(home, "/p"))), (other, Some(place(elsewhere, "/p")))], 62_000, Some(60_000));
        assert!(seen.alone(me, &place(home, "/p"), 60_000));
        seen.note(vec![(me, Some(place(home, "/p"))), (other, Some(place(home, "/p")))], 63_000, Some(60_000));
        assert!(!seen.alone(me, &place(home, "/p"), 60_000));
        // it exits; it was there after this one started
        seen.note(vec![(me, Some(place(home, "/p")))], 64_000, Some(60_000));
        assert!(!seen.alone(me, &place(home, "/p"), 60_000));
    }
}

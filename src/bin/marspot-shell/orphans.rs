//! L3 processes of this install that no registry entry names.
//!
//! A spawn race can leave an L3 whose `entry.toml` a later sibling
//! overwrote: nothing will ever reattach to it, so quitting stops it.
//! "Of this install" is two facts read from the kernel, not from the
//! command line: the process runs one of the `marspot-session`
//! executables this install starts, and its `MARSPOT_STATE_DIR` names
//! our state root.  The L3s of another marspot (a sandbox, a second
//! checkout) fail the second, and a process that only mentions
//! `marspot-session` in its arguments fails the first.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use marspot::lx_warn;
use marspot::pidtree::{self, ProcId, ProcRow};
use crate::supervisor::BinaryTree;
use marspot_term::paths;

const L3: &str = "marspot-session";

pub struct Install {
    /// Canonical paths of the L3 executables this install can be running.
    exes: Vec<PathBuf>,
    state_root: PathBuf,
}

impl Install {
    /// The install this process belongs to.  L2 starts an L3 from
    /// `MARSPOT_SESSION_BIN` or from next to its own executable, and an
    /// L3 that updated itself runs from `binaries/current/`; L2 is
    /// whatever `cores` resolves, or was the bundle's copy next to L1.
    pub fn ours(cores: &BinaryTree) -> Install {
        let mut exes = vec![paths::binaries_root().join("current").join(L3)];
        if let Ok(me) = std::env::current_exe() {
            exes.push(cores.resolve_runnable(&me).with_file_name(L3));
            exes.push(me.with_file_name(L3));
        }
        if let Some(p) = std::env::var_os("MARSPOT_SESSION_BIN") {
            exes.push(p.into());
        }
        Install::new(exes, &paths::state_root())
    }

    pub fn new(exes: Vec<PathBuf>, state_root: &Path) -> Install {
        Install {
            exes: exes.iter().map(|p| canonical(p)).collect(),
            state_root: canonical(state_root),
        }
    }

    /// Whether a process running `exe`, whose `MARSPOT_STATE_DIR` is
    /// `state_dir` (None: unset), belongs to this install.
    fn owns(&self, exe: &Path, state_dir: Option<&str>) -> bool {
        let root = state_dir.map_or_else(paths::default_state_root, PathBuf::from);
        self.exes.contains(&canonical(exe)) && canonical(&root) == self.state_root
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A process's `MARSPOT_STATE_DIR`: `Some(None)` when it is unset,
/// `None` when the process cannot be read at all.
fn state_dir_of(pid: i32) -> Option<Option<String>> {
    // argv and the environment come from one sysctl, so a readable argv
    // tells "unset" apart from "not allowed to look"
    pidtree::proc_argv(pid)?;
    Some(pidtree::proc_env_value(pid, "MARSPOT_STATE_DIR"))
}

/// Whether `pid` runs with this process's state directory.  False when
/// its environment cannot be read.
pub fn shares_our_state_dir(pid: i32) -> bool {
    state_dir_of(pid).is_some_and(|d| {
        let root = d.map_or_else(paths::default_state_root, PathBuf::from);
        canonical(&root) == canonical(&paths::state_root())
    })
}

/// The L3s of `install` among `rows` whose pid no registry entry names.
pub fn orphans<'a>(
    rows: impl IntoIterator<Item = &'a ProcRow>,
    registered: &HashSet<i32>,
    install: &Install,
    exe: impl Fn(i32) -> Option<PathBuf>,
    state_dir: impl Fn(i32) -> Option<Option<String>>,
) -> Vec<ProcId> {
    rows.into_iter()
        // comm is the executable's file name, so this only skips the
        // processes the exe check below would reject anyway
        .filter(|r| r.comm == L3 && !registered.contains(&r.pid))
        .filter(|r| {
            exe(r.pid)
                .zip(state_dir(r.pid))
                .is_some_and(|(x, d)| install.owns(&x, d.as_deref()))
        })
        .map(ProcRow::id)
        .collect()
}

/// SIGTERM every orphaned L3 of `install`.  Returns how many sessions
/// the registry keeps and how many orphans were signalled.
pub fn stop(install: &Install) -> (usize, usize) {
    let obs = crate::observe::shared().fresh(crate::observe::NOW);
    let registered: HashSet<i32> = obs.sessions.iter().map(|s| s.entry.pid).collect();
    let found = orphans(
        obs.table.rows(),
        &registered,
        install,
        marspot_term::platform::pid_exe_path,
        state_dir_of,
    );
    let signalled = found
        .into_iter()
        .filter(|id| match pidtree::signal(*id, libc::SIGTERM) {
            Ok(()) => true,
            Err(e) => {
                lx_warn!("shell.quit.orphan_signal_failed", &format!("{e}"), pid = id.pid);
                false
            }
        })
        .count();
    (obs.sessions.len(), signalled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    const HELPER_ENV: &str = "MARSPOT_TEST_ORPHAN_HELPER";

    /// Not a test: the body a copy of this test binary runs when the
    /// tests below need a process with an executable of their choosing.
    /// A copied system binary is no use for that: launch constraints
    /// kill it at exec.
    #[test]
    #[ignore]
    fn helper_sleeps() {
        if std::env::var_os(HELPER_ENV).is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    fn run_helper(exe: &Path, state_dir: Option<&Path>) -> Child {
        let mut cmd = Command::new(exe);
        cmd.args(["--ignored", "--exact", "orphans::tests::helper_sleeps", "--test-threads=1"])
            .env(HELPER_ENV, "1")
            .env_remove("MARSPOT_STATE_DIR");
        if let Some(d) = state_dir {
            cmd.env("MARSPOT_STATE_DIR", d);
        }
        cmd.spawn().expect("spawn the helper")
    }

    /// The table once `pid` runs an executable named `comm`.
    fn table_when(pid: i32, comm: &str) -> pidtree::ProcTable {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = pidtree::ProcTable::take();
            if t.get(pid).is_some_and(|r| r.comm == comm) {
                return t;
            }
            assert!(Instant::now() < deadline, "pid {pid} never ran {comm}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn real(
        t: &pidtree::ProcTable,
        install: &Install,
        pids: &[i32],
    ) -> Vec<i32> {
        let rows = t.rows().iter().filter(|r| pids.contains(&r.pid));
        let mut got: Vec<i32> = orphans(
            rows,
            &HashSet::new(),
            install,
            marspot_term::platform::pid_exe_path,
            state_dir_of,
        )
        .into_iter()
        .map(|id| id.pid)
        .collect();
        got.sort_unstable();
        got
    }

    #[test]
    fn only_an_l3_executable_of_ours_with_our_state_dir_is_an_orphan() {
        let dir = std::env::temp_dir().join(format!("marspot-orphans-{}", std::process::id()));
        let ours_root = dir.join("ours");
        let theirs_root = dir.join("theirs");
        std::fs::create_dir_all(&ours_root).unwrap();
        std::fs::create_dir_all(&theirs_root).unwrap();
        let l3 = dir.join(L3);
        std::fs::copy(std::env::current_exe().unwrap(), &l3).unwrap();
        let install = Install::new(vec![l3.clone()], &ours_root);

        // pgrep -f matches this one: argv[0] says marspot-session
        let mut named = Command::new("/bin/sleep").arg0(L3).arg("30").spawn().unwrap();
        let mut theirs = run_helper(&l3, Some(&theirs_root));
        let mut mine = run_helper(&l3, Some(&ours_root));
        let pids = [named.id() as i32, theirs.id() as i32, mine.id() as i32];
        table_when(pids[0], "sleep");
        table_when(pids[1], L3);
        let t = table_when(pids[2], L3);
        let got = real(&t, &install, &pids);
        let argv = pidtree::proc_argv(pids[0]);
        for c in [&mut named, &mut theirs, &mut mine] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(argv.as_ref().and_then(|a| a.first()).map(String::as_str), Some(L3));
        assert_eq!(got, vec![pids[2]], "only the L3 of our state dir; named={} theirs={}", pids[0], pids[1]);
    }

    fn row(pid: i32, comm: &str) -> ProcRow {
        crate::observe::tests::row(pid, 1, comm)
    }

    #[test]
    fn registered_unreadable_and_default_state_dirs_are_decided_by_the_rules() {
        let l3 = PathBuf::from("/opt/m/marspot-session");
        let rows = [row(10, L3), row(11, L3), row(12, L3), row(13, L3), row(14, "zsh")];
        let exe = |pid: i32| Some(if pid == 13 { PathBuf::from("/x/marspot-session") } else { l3.clone() });
        let state = |pid: i32| match pid {
            11 => None,
            _ => Some(None),
        };
        let registered: HashSet<i32> = [12].into();
        let default = Install::new(vec![l3.clone()], &paths::default_state_root());
        let got: Vec<i32> =
            orphans(&rows, &registered, &default, exe, state).into_iter().map(|i| i.pid).collect();
        assert_eq!(got, vec![10], "11 unreadable, 12 registered, 13 another executable, 14 not an L3");
        let sandbox = Install::new(vec![l3.clone()], Path::new("/nonexistent/sandbox"));
        assert!(
            orphans(&rows, &registered, &sandbox, exe, state).is_empty(),
            "a sandbox does not own L3s that have no MARSPOT_STATE_DIR"
        );
    }

    /// What `--trigger` relies on before sending SIGUSR1, which ends a
    /// process that does not handle it.  The helper, not `/bin/sleep`:
    /// a platform binary's environment cannot be read at all.
    #[test]
    fn only_a_process_with_our_state_dir_shares_it() {
        let me = std::env::current_exe().expect("test binary");
        let other = std::env::temp_dir().join(format!("marspot-other-state-{}", std::process::id()));
        let mut same = run_helper(&me, Some(&paths::state_root()));
        let mut different = run_helper(&me, Some(&other));
        // until exec the child still has our environment
        let execed = |c: &Child| pidtree::proc_env_value(c.id() as i32, HELPER_ENV).is_some();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(execed(&same) && execed(&different)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let (s, d) = (shares_our_state_dir(same.id() as i32), shares_our_state_dir(different.id() as i32));
        for c in [&mut same, &mut different] {
            let _ = c.kill();
            let _ = c.wait();
        }
        assert!(s, "a process run with our state dir shares it");
        assert!(!d, "a process run with another state dir does not");
    }
}

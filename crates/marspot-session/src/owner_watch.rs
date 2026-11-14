//! A session keeps its coalition answered for.
//!
//! The app's shell is what LaunchServices holds answerable for the
//! coalition the sessions were born in (`marspot_term::owner`), and it
//! stays for as long as a session does.  When it dies anyway, a
//! process outside the coalition cannot take its place: a shell the
//! user starts afterwards is in a coalition of its own.  A session can,
//! because what it starts is born where it is.  So every session
//! watches the shell on record, and when that one exits without a
//! successor, one of them starts a shell with no window to answer for
//! them all until the user opens the app again.  The same is done on
//! request for a shell that came up outside: it asks for an heir in the
//! sessions' coalition and leaves.
//!
//! Nothing here runs on a timer or on a thread of its own: the record
//! and the shell's pid are two more things the session's file watcher
//! waits on (`upkeep`), and `look` runs on the main loop when either
//! changes.

use marspot_term::fs_watch::Watcher;

/// Starts of a shell in the last minute that stop another one: a shell
/// that dies as it comes up would otherwise be started again for ever.
/// Two kinds are counted apart.  A shell started because the one that
/// answered is gone is the kind that can die in a loop.  One that a
/// living shell asked for comes up, finds whether it is given the
/// coalition, and leaves quietly when it is not: that is no crash, and
/// must not cost a shell that dies right after it its successor.
// off macOS no shell is started: this is built for its tests only
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const STARTS_A_MINUTE: usize = 3;

/// How a start asked for by a living shell is written in the file.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const ASKED_MARK: char = 'a';

/// The lines of `text` still inside the minute before `now`, with one
/// more for a start of this kind when that is allowed; `None` when it
/// is not.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn with_one_more_start(text: &str, now: u64, asked: bool) -> Option<String> {
    let recent: Vec<(bool, u64)> = text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let asked = l.starts_with(ASKED_MARK);
            Some((asked, l.trim_start_matches(ASKED_MARK).parse().ok()?))
        })
        .filter(|(_, t)| now.saturating_sub(*t) < 60 && *t <= now)
        .collect();
    if recent.iter().filter(|(kind, _)| *kind == asked).count() >= STARTS_A_MINUTE {
        return None;
    }
    let line = |(asked, t): &(bool, u64)| if *asked { format!("{ASKED_MARK}{t}\n") } else { format!("{t}\n") };
    Some(recent.iter().chain(std::iter::once(&(asked, now))).map(line).collect())
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct OwnerWatch;

#[cfg(not(target_os = "macos"))]
impl OwnerWatch {
    pub(crate) fn new(_sid: u64) -> Self {
        OwnerWatch
    }
    pub(crate) fn look(&mut self, _watcher: Option<&Watcher>) {}
}

#[cfg(target_os = "macos")]
pub(crate) use macos::OwnerWatch;

#[cfg(target_os = "macos")]
mod macos {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use marspot_term::owner::{self, Record};
    use marspot_term::{lx_event, lx_warn, platform, session_registry};

    use super::{Watcher, with_one_more_start};

    /// What the core and this session's own hand-over put in the
    /// environment for a session alone.
    const SESSION_ONLY_ENV: &[&str] = &[
        "MARSPOT_SESSION_ID",
        "MARSPOT_SHM_NAME",
        "MARSPOT_INITIAL_CWD",
        marspot_term::grid_shm::ENV_SHM_FD,
        crate::handoff::ENV_HANDOFF_MANIFEST,
        marspot_term::shell_proto::ENV_SURFACE_ID,
        marspot_term::shell_proto::ENV_SURFACE_ID_BACK,
        marspot_term::shell_proto::ENV_SURFACE_WIDTH,
        marspot_term::shell_proto::ENV_SURFACE_HEIGHT,
        marspot_term::shell_proto::ENV_SURFACE_SCALE,
        marspot_term::shell_proto::ENV_CONTROL_FD,
    ];

    pub(crate) struct OwnerWatch {
        sid: u64,
        coalition: Option<u64>,
        /// The shell whose exit the watcher was asked to report.
        watched: Option<i32>,
        /// What the last `SESSION_OWNER` line said.
        said: Option<Option<i32>>,
        /// Held from starting a shell until one is on record or the
        /// one started has exited: one session starts it, not each.
        turn: Option<(File, i32)>,
        /// The shells this session started that have not been
        /// collected: they are its children for as long as it lives.
        started: Vec<i32>,
    }

    impl OwnerWatch {
        pub(crate) fn new(sid: u64) -> OwnerWatch {
            // says that this session can start a shell when asked
            let _ = std::fs::write(session_registry::session_dir(sid).join("guard"), b"1\n");
            let coalition = platform::coalition_of(std::process::id() as i32);
            OwnerWatch { sid, coalition, watched: None, said: None, turn: None, started: Vec::new() }
        }

        fn say(&mut self, owner_pid: Option<i32>) {
            if self.said == Some(owner_pid) {
                return;
            }
            self.said = Some(owner_pid);
            lx_event!(
                "SESSION_OWNER",
                "who answers for this session's coalition",
                sid = self.sid,
                coalition = self.coalition.map_or_else(|| "unknown".to_string(), |c| c.to_string()),
                owner_pid = owner_pid.map_or_else(|| "none".to_string(), |p| p.to_string())
            );
        }

        /// Call when the record or a watched pid changed.  `watcher`
        /// is asked to report the exit of the shell on record.
        pub(crate) fn look(&mut self, watcher: Option<&Watcher>) {
            // SAFETY: collects a child of this session, if it has exited.
            self.started.retain(|pid| unsafe { libc::waitpid(*pid, std::ptr::null_mut(), libc::WNOHANG) } == 0);
            if self.turn.as_ref().is_some_and(|(_, pid)| !self.started.contains(pid)) {
                self.turn = None;
            }
            let Some(r) = Record::read() else {
                // no record at all; one caught between its truncation
                // and its write is reported again when it is written
                if !owner::path().exists() {
                    self.say(None);
                }
                return;
            };
            let watching = |pid: i32| watcher.is_none_or(|w| w.watch_pid(pid).is_ok());
            if r.alive() && (self.watched == Some(r.pid) || watching(r.pid)) {
                if self.watched != Some(r.pid) {
                    self.watched = Some(r.pid);
                    self.turn = None;
                }
                self.say(owner::owner_of(self.coalition, Some(&r)));
                if r.heir_in.is_some() && r.heir_in == self.coalition {
                    self.start_shell(&r, Some(r.pid), watcher);
                }
                return;
            }
            self.watched = None;
            self.say(None);
            if r.coalition.is_some() && r.coalition == self.coalition && self.turn.is_none() {
                lx_event!("OWNER_GONE", "the shell that answered for this coalition is gone", sid = self.sid, was = r.pid);
                self.start_shell(&r, None, watcher);
            }
        }

        /// Start a shell here, if no other session is doing so: one
        /// with no window, or the heir of `heir_of`.
        fn start_shell(&mut self, r: &Record, heir_of: Option<i32>, watcher: Option<&Watcher>) {
            if self.turn.is_some() {
                return;
            }
            let Ok(mut turn) = File::options().read(true).write(true).create(true).truncate(false).open(owner::spawn_lock_path()) else {
                return;
            };
            // SAFETY: flock on a descriptor this function owns.
            if unsafe { libc::flock(turn.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return;
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let mut text = String::new();
            let _ = turn.read_to_string(&mut text);
            let Some(starts) = with_one_more_start(&text, now, heir_of.is_some()) else {
                lx_warn!(
                    "OWNER_START_HELD",
                    "a shell was started here three times in a minute; not again",
                    sid = self.sid,
                    asked_for = heir_of.is_some()
                );
                return;
            };
            let _ = turn.set_len(0);
            let _ = turn.seek(SeekFrom::Start(0));
            let _ = turn.write_all(starts.as_bytes());

            let exe = owner::shell_to_start(&r.exe);
            let mut cmd = Command::new(&exe);
            for name in SESSION_ONLY_ENV {
                cmd.env_remove(name);
            }
            // the dev seams were for the app that started this session
            for (name, _) in std::env::vars_os() {
                if name.to_str().is_some_and(|n| n.starts_with("MARSPOT_DEV_") && n != "MARSPOT_DEV_SANDBOX") {
                    cmd.env_remove(name);
                }
            }
            match heir_of {
                None => cmd.env(owner::RESIDENT_ENV, owner::RESIDENT_STARTED),
                Some(pid) => cmd.env(owner::HEIR_ENV, pid.to_string()),
            };
            if heir_of.is_some() && r.heir_resident {
                cmd.env(owner::RESIDENT_ENV, "1");
            }
            if heir_of.is_some() {
                // what the shell that asks hands over: its own settings, nothing else
                cmd.envs(r.heir_env.iter().filter(|(k, _)| k.starts_with("MARSPOT_")).map(|(k, v)| (k, v)));
            }
            // a group of its own: the pane's hangup is not for it
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
            let limit = marspot_term::inherited_fds::close_limit();
            // SAFETY: only fcntl between fork and exec.  The shell must
            // not hold this pane's terminal or frame region open.
            unsafe {
                cmd.pre_exec(move || {
                    marspot_term::inherited_fds::close_inherited(limit, &[]);
                    Ok(())
                });
            }
            match cmd.spawn() {
                Ok(child) => {
                    let pid = child.id() as i32;
                    // its exit is collected in `look`, whether it became the shell or not
                    if let Some(w) = watcher {
                        let _ = w.watch_pid(pid);
                    }
                    lx_event!(
                        "OWNER_STARTED",
                        "started a shell in this coalition to answer for it",
                        sid = self.sid,
                        pid = pid,
                        heir_of = heir_of.unwrap_or(0),
                        exe = exe.display()
                    );
                    self.started.push(pid);
                    self.turn = Some((turn, pid));
                }
                Err(e) => lx_warn!("OWNER_START_FAILED", &format!("{e}"), sid = self.sid, exe = exe.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fourth_start_in_a_minute_is_refused_and_old_ones_are_forgotten() {
        let now = 1_000_000;
        assert_eq!(with_one_more_start("", now, false), Some(format!("{now}\n")));
        let text = format!("{}\n{}\n{}\n", now - 100, now - 30, now - 2);
        assert_eq!(with_one_more_start(&text, now, false), Some(format!("{}\n{}\n{now}\n", now - 30, now - 2)));
        let text = format!("{}\n{}\n{}\n", now - 50, now - 30, now - 2);
        assert_eq!(with_one_more_start(&text, now, false), None);
        assert_eq!(with_one_more_start("junk\n\n", now, false), Some(format!("{now}\n")));
    }

    #[test]
    fn shells_a_living_shell_asked_for_do_not_cost_a_dead_one_its_successor() {
        let now = 1_000_000;
        let asked = format!("a{}\na{}\na{}\n", now - 9, now - 6, now - 3);
        // three that were asked for, and the shell dies: one is started
        assert_eq!(with_one_more_start(&asked, now, false), Some(format!("{asked}{now}\n")));
        // a fourth that is asked for is not
        assert_eq!(with_one_more_start(&asked, now, true), None);
        // and three that died do not stop one that is asked for
        let died = format!("{}\n{}\n{}\n", now - 9, now - 6, now - 3);
        assert_eq!(with_one_more_start(&died, now, true), Some(format!("{died}a{now}\n")));
        assert_eq!(with_one_more_start(&died, now, false), None);
    }
}

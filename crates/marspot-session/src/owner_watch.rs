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
//! Nothing here runs on a timer: the thread sleeps in `kevent` on the
//! record and on the shell's pid.

/// Started once per image: at boot and again after an update replaced
/// the image.  What the watch holds open is open when this returns.
pub(crate) fn start(sid: u64) {
    #[cfg(target_os = "macos")]
    {
        let mut watch = macos::Watch::new(sid);
        let found = watch.open_record();
        if let Err(e) = std::thread::Builder::new().name("l3-owner".into()).spawn(move || watch.run(found)) {
            marspot_term::lx_warn!("l3.owner.unwatched", &format!("{e}"), session_id = sid);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = sid;
}

/// Starts of a shell in the last minute that stop another one: a shell
/// that dies as it comes up would otherwise be started again for ever.
// off macOS no shell is started: this is built for its tests only
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const STARTS_A_MINUTE: usize = 3;

/// The start times still inside the minute before `now`, and whether
/// one more start is allowed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn recent_starts(text: &str, now: u64) -> (Vec<u64>, bool) {
    let recent: Vec<u64> =
        text.lines().filter_map(|l| l.trim().parse().ok()).filter(|t| now.saturating_sub(*t) < 60 && *t <= now).collect();
    let allowed = recent.len() < STARTS_A_MINUTE;
    (recent, allowed)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use marspot_term::owner::{self, Record};
    use marspot_term::{lx_event, lx_warn, platform, session_registry};

    use super::recent_starts;

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

    pub(super) struct Watch {
        sid: u64,
        kq: RawFd,
        coalition: Option<u64>,
        /// The record, open for as long as it exists.
        file: Option<File>,
        /// The state root, watched until the record exists.
        dir: Option<File>,
        watched: Option<i32>,
        /// What the last `SESSION_OWNER` line said.
        said: Option<Option<i32>>,
        /// Held from starting a shell until one is on record or the
        /// one started has exited: one session starts it, not each.
        turn: Option<(File, i32)>,
    }

    impl Watch {
        pub(super) fn new(sid: u64) -> Watch {
            // SAFETY: kqueue takes no arguments; the flag keeps the
            // queue out of the image an update moves this process to.
            let kq = unsafe {
                let kq = libc::kqueue();
                libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
                kq
            };
            let coalition = platform::coalition_of(std::process::id() as i32);
            // says that this session can start a shell when asked
            let _ = std::fs::write(session_registry::session_dir(sid).join("guard"), b"1\n");
            Watch { sid, kq, coalition, file: None, dir: None, watched: None, said: None, turn: None }
        }

        fn add(&self, ident: usize, filter: i16, fflags: u32) -> bool {
            let ev = libc::kevent {
                ident,
                filter,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one change, no events asked for.
            unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 }
        }

        /// Open the record if it is there; otherwise watch for it.
        pub(super) fn open_record(&mut self) -> bool {
            if self.file.is_some() {
                return true;
            }
            match File::open(owner::path()) {
                Ok(f) => {
                    let changes = libc::NOTE_WRITE | libc::NOTE_ATTRIB | libc::NOTE_DELETE | libc::NOTE_RENAME;
                    self.add(f.as_raw_fd() as usize, libc::EVFILT_VNODE, changes);
                    self.file = Some(f);
                    self.dir = None;
                    true
                }
                Err(_) => {
                    if self.dir.is_none()
                        && let Ok(d) = File::open(marspot_term::paths::state_root())
                    {
                        self.add(d.as_raw_fd() as usize, libc::EVFILT_VNODE, libc::NOTE_WRITE);
                        self.dir = Some(d);
                    }
                    false
                }
            }
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

        fn look(&mut self) {
            // caught between its truncation and its write: the write is the next event
            let Some(r) = Record::read() else { return };
            if r.alive() && (self.watched == Some(r.pid) || self.add(r.pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT)) {
                if self.watched != Some(r.pid) {
                    self.watched = Some(r.pid);
                    self.turn = None;
                }
                self.say(owner::owner_of(self.coalition, Some(&r)));
                if r.heir_in.is_some() && r.heir_in == self.coalition {
                    self.start_shell(&r, Some(r.pid));
                }
                return;
            }
            self.watched = None;
            self.say(None);
            if r.coalition.is_some() && r.coalition == self.coalition {
                lx_event!("OWNER_GONE", "the shell that answered for this coalition is gone", sid = self.sid, was = r.pid);
                self.start_shell(&r, None);
            }
        }

        /// Start a shell here, if no other session is doing so: one
        /// with no window, or the heir of `heir_of`.
        fn start_shell(&mut self, r: &Record, heir_of: Option<i32>) {
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
            let (mut starts, allowed) = recent_starts(&text, now);
            if !allowed {
                lx_warn!("OWNER_START_HELD", "a shell was started here three times in a minute; not again", sid = self.sid);
                return;
            }
            starts.push(now);
            let _ = turn.set_len(0);
            let _ = turn.seek(SeekFrom::Start(0));
            let _ = turn.write_all(starts.iter().map(|t| format!("{t}\n")).collect::<String>().as_bytes());

            let mut cmd = Command::new(&r.exe);
            for name in SESSION_ONLY_ENV {
                cmd.env_remove(name);
            }
            // the dev seams were for the app that started this session
            for (name, _) in std::env::vars_os() {
                if name.to_str().is_some_and(|n| n.starts_with("MARSPOT_DEV_") && n != "MARSPOT_DEV_SANDBOX") {
                    cmd.env_remove(name);
                }
            }
            if let Some(pid) = heir_of {
                cmd.env(owner::HEIR_ENV, pid.to_string());
            }
            match heir_of {
                None => cmd.env(owner::RESIDENT_ENV, owner::RESIDENT_STARTED),
                Some(_) if r.heir_resident => cmd.env(owner::RESIDENT_ENV, "1"),
                Some(_) => &mut cmd,
            };
            // a session of its own: the pane's hangup is not for it
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
                    // its exit is collected here, whether it became the shell or not
                    self.add(child.id() as usize, libc::EVFILT_PROC, libc::NOTE_EXIT);
                    lx_event!(
                        "OWNER_STARTED",
                        "started a shell in this coalition to answer for it",
                        sid = self.sid,
                        pid = child.id(),
                        heir_of = heir_of.unwrap_or(0),
                        exe = r.exe.display()
                    );
                    self.turn = Some((turn, child.id() as i32));
                }
                Err(e) => lx_warn!("OWNER_START_FAILED", &format!("{e}"), sid = self.sid, exe = r.exe.display()),
            }
        }

        pub(super) fn run(mut self, found: bool) {
            if found {
                self.look();
            } else {
                self.say(None);
            }
            loop {
                // SAFETY: an all-zero kevent is a valid one to be filled in.
                let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
                // SAFETY: room for one event, no timeout.
                let n = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
                if n < 1 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return lx_warn!("l3.owner.wait_failed", &format!("{}", std::io::Error::last_os_error()), session_id = self.sid);
                }
                if ev.filter == libc::EVFILT_PROC {
                    let pid = ev.ident as i32;
                    // SAFETY: collects a child of ours that has exited; for any other pid it fails.
                    unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                    if self.watched == Some(pid) {
                        self.watched = None;
                    }
                    if self.turn.as_ref().is_some_and(|(_, started)| *started == pid) {
                        self.turn = None;
                    }
                    if self.open_record() {
                        self.look();
                    }
                } else {
                    // removed or replaced: the descriptor is of a file nobody writes any more
                    if ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                        self.file = None;
                    }
                    if self.open_record() {
                        self.look();
                    }
                }
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
        assert_eq!(recent_starts("", now), (vec![], true));
        let text = format!("{}\n{}\n{}\n", now - 100, now - 30, now - 2);
        assert_eq!(recent_starts(&text, now), (vec![now - 30, now - 2], true));
        let text = format!("{}\n{}\n{}\n", now - 50, now - 30, now - 2);
        assert_eq!(recent_starts(&text, now), (vec![now - 50, now - 30, now - 2], false));
        assert_eq!(recent_starts("junk\n\n", now), (vec![], true));
    }
}

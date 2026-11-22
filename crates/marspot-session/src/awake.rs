//! What the session tells its core, and through the core the shell,
//! about its pane: that it printed, and that another process group has
//! the terminal (`MsgType::PaneAwake`).  Without these the shell finds
//! both out for itself, once a second, for as long as any pane is not a
//! shell sitting at its prompt.
//!
//! Neither costs anything while the pane is quiet, and nothing per
//! byte.  Output is reported from the loop's turn that parsed it, at
//! most once per [`OUTPUT_EVERY`]: the first turn after a quiet spell
//! reports at once, and a pane that keeps printing reports again on its
//! first turn after each [`OUTPUT_EVERY`].  So a report says "printed
//! at this moment"; the last output of a burst can be up to
//! `OUTPUT_EVERY` later than the last report.
//!
//! The foreground group is read (`tcgetpgrp` on the master) on those
//! same turns and when the shell's processes change.  There is no event
//! for the terminal changing hands, but the shell hands it over in the
//! child it forked, before that child execs the command (zsh and bash
//! both), and takes it back after the job's processes exit, before its
//! prompt prints.  So a kqueue waits for the shell to fork, and for the
//! children it forked to exec or exit, and each of those is a moment to
//! look.  A job resumed with `fg` that prints nothing is seen only when
//! it next prints or ends.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{Frame, MsgType, PaneAwake, encode_pane_awake};

use crate::SessionEvent;
use crate::control_writer::ControlWriter;

/// The longest a pane that keeps printing goes between two reports.
pub(crate) const OUTPUT_EVERY: Duration = Duration::from_secs(1);

/// The children of the shell looked at per fork.  A shell's children
/// are its running jobs and command substitutions: a handful.
const CHILDREN_MAX: usize = 64;

pub(crate) struct Awake {
    sid: u64,
    master: RawFd,
    shell: libc::pid_t,
    output_reported: Option<Instant>,
    /// The foreground group last reported; -1 before the first report
    /// to this core.
    fg: libc::pid_t,
    /// Set by the watcher thread when it has a `ProcsChanged` queued,
    /// so a shell that forks ten times for one prompt queues one.
    queued: Arc<AtomicBool>,
}

impl Awake {
    /// Start watching the shell's processes; without the watch, output
    /// is still reported, and a change of foreground with it.
    pub(crate) fn new(sid: u64, master: RawFd, shell: libc::pid_t, tx: Sender<SessionEvent>) -> Self {
        let queued = Arc::new(AtomicBool::new(false));
        if let Err(e) = watch_procs(shell, Arc::clone(&queued), tx) {
            marspot_term::lx_warn!("l3.awake.unwatched", &format!("{e}"), session_id = sid, shell_pid = shell);
        }
        Self { sid, master, shell, output_reported: None, fg: -1, queued }
    }

    /// The `ProcsChanged` the watcher queued has been taken.
    pub(crate) fn procs_seen(&self) {
        self.queued.store(false, Ordering::Release);
    }

    /// A core connected: it hears where things stand from the first
    /// look, whatever was reported to the one before it.
    pub(crate) fn new_core(&mut self, poke: Option<&mut ControlWriter>) {
        self.fg = -1;
        self.output_reported = None;
        self.after_turn(false, true, poke);
    }

    /// After a turn of the loop: `printed` when it parsed output,
    /// `look` when the shell's processes changed.
    pub(crate) fn after_turn(&mut self, printed: bool, look: bool, poke: Option<&mut ControlWriter>) {
        if !printed && !look {
            return;
        }
        let Some(w) = poke else { return };
        let now = Instant::now();
        let output = printed && self.output_reported.is_none_or(|t| now.duration_since(t) >= OUTPUT_EVERY);
        // the foreground is read only on a turn that reports, so a busy
        // pty pays no extra syscall per read
        if !output && !look {
            return;
        }
        // SAFETY: tcgetpgrp on the pty master this session owns.
        let fg = unsafe { libc::tcgetpgrp(self.master) }.max(0);
        let foreground = fg != self.fg;
        if !output && !foreground {
            return;
        }
        // SAFETY: getpgid only reads the process table.
        let shell_in_front = fg > 0 && fg == unsafe { libc::getpgid(self.shell) };
        let said = PaneAwake { session_id: self.sid, output, foreground, shell_in_front, fg_pgid: fg };
        if !w.send(Frame::new(MsgType::PaneAwake, encode_pane_awake(&said))) {
            return;
        }
        if output {
            self.output_reported = Some(now);
        }
        if foreground {
            marspot_term::lx_debug!(
                "l3.awake.foreground",
                "the terminal's foreground group changed",
                session_id = self.sid,
                fg_pgid = fg,
                shell_in_front = shell_in_front as u32
            );
        }
        self.fg = fg;
    }
}

fn proc_change(kq: RawFd, pid: libc::pid_t, fflags: u32) -> bool {
    // SAFETY: an all-zero kevent is a valid value.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = pid as libc::uintptr_t;
    ev.filter = libc::EVFILT_PROC;
    // the kernel drops the note when the process exits
    ev.flags = libc::EV_ADD | libc::EV_CLEAR;
    ev.fflags = fflags;
    // SAFETY: one change, no events asked for.  Refused for a process
    // already gone, which then has nothing left to report.
    unsafe { libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 }
}

/// Watch every child `shell` has now for its exec and exit.  Adding a
/// note a child already has changes nothing.
fn watch_children(kq: RawFd, shell: libc::pid_t) {
    let mut pids = [0 as libc::pid_t; CHILDREN_MAX];
    let size = std::mem::size_of_val(&pids) as libc::c_int;
    // SAFETY: the buffer is `size` bytes of pids.
    if unsafe { libc::proc_listchildpids(shell, pids.as_mut_ptr().cast(), size) } < 0 {
        return;
    }
    for pid in pids.into_iter().filter(|p| *p > 0) {
        proc_change(kq, pid, libc::NOTE_EXEC | libc::NOTE_EXIT);
    }
}

fn watch_procs(shell: libc::pid_t, queued: Arc<AtomicBool>, tx: Sender<SessionEvent>) -> io::Result<()> {
    // SAFETY: kqueue returns a new descriptor or -1.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a descriptor nothing else owns.
    let kq = unsafe {
        libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
        OwnedFd::from_raw_fd(raw)
    };
    if !proc_change(kq.as_raw_fd(), shell, libc::NOTE_FORK | libc::NOTE_EXIT) {
        return Err(io::Error::last_os_error());
    }
    // after an execv the shell may be running a job already
    watch_children(kq.as_raw_fd(), shell);
    std::thread::Builder::new().name("l3-awake-procs".into()).spawn(move || {
        // SAFETY: an all-zero kevent is a valid value.
        let mut got: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: room for `got.len()` events; no timeout.
            let n = unsafe {
                libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, got.as_mut_ptr(), got.len() as libc::c_int, std::ptr::null())
            };
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            let mut shell_gone = false;
            for ev in &got[..n as usize] {
                if ev.ident == shell as libc::uintptr_t {
                    if ev.fflags & libc::NOTE_FORK != 0 {
                        watch_children(kq.as_raw_fd(), shell);
                    }
                    shell_gone |= ev.fflags & libc::NOTE_EXIT != 0;
                }
            }
            if !queued.swap(true, Ordering::AcqRel) && tx.send(SessionEvent::ProcsChanged).is_err() {
                return;
            }
            if shell_gone {
                return;
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc;

    /// A fork, an exec and an exit of a child of the watched process
    /// each queue a look, and while one is queued no more are.
    #[test]
    fn a_child_started_and_ended_is_seen_and_looks_are_coalesced() {
        let mut shell = Command::new("/bin/sh").args(["-c", "read _; /bin/sleep 0.2; read _"]).stdin(std::process::Stdio::piped()).spawn().unwrap();
        let (tx, rx) = mpsc::channel();
        let queued = Arc::new(AtomicBool::new(false));
        watch_procs(shell.id() as i32, Arc::clone(&queued), tx).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "a look with nothing changed");
        use std::io::Write;
        let stdin = shell.stdin.as_mut().unwrap();
        stdin.write_all(b"\n").unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(SessionEvent::ProcsChanged)), "the fork was not seen");
        // not taken yet: whatever else happened is in this one
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_recv().is_err(), "a second look queued while one was");
        queued.store(false, Ordering::Release);
        stdin.write_all(b"\n").unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(SessionEvent::ProcsChanged)), "the shell's exit was not seen");
        let _ = shell.wait();
    }
}

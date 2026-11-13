//! Taking the sessions' coalition back when it is listed under
//! another program's record.
//!
//! A shell that takes the place of one that died registers some tens
//! of milliseconds after the dead shell's record went (`identity`),
//! and a program a pane starts in that time is given the coalition
//! instead.  Measured on macOS 27.2.1, for as long as that program
//! runs nothing gets the coalition back: not this app registering, or
//! registering again, or being started once more by LaunchServices.
//! So this waits for the program to exit, asleep on its pid, and then
//! has a new shell started in this one's place: a coalition is given
//! to a record when the record is made, never to one that exists.
//!
//! A program that exits with the coalition leaves a dead record that
//! keeps it, for as long as a session is left.  Nothing found takes
//! that record over but the same program run again (measured: this app
//! registering in any way, another program, a file of the same name, a
//! process that claims the program's path).  It does keep any other
//! program from being given the sessions.  When the program is run
//! again its old record goes, and the app is told that an application
//! appeared: that is when it looks again.

use marspot::{lx_event, lx_warn};
use marspot_term::platform;

use crate::ls_records::{self, Holder};
use crate::ownership;

/// Counts the shells started one after another to be given the
/// coalition, for the next one: a state that a new shell does not
/// change is left alone after the third.
const RESEATS_ENV: &str = "MARSPOT_SHELL_RESEATS";
static RESEATS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LOOKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A dead record of another program has the coalition.
static KEPT_BY_DEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called when the system says an application appeared: the one thing
/// that takes a dead record over is its program running again.
pub(crate) fn application_appeared() {
    if KEPT_BY_DEAD.swap(false, std::sync::atomic::Ordering::AcqRel) {
        LOOKING.store(false, std::sync::atomic::Ordering::Release);
        // the record goes when the program registers, which is about now
        watch_in(std::time::Duration::from_millis(500));
    }
}

pub(crate) fn watch() {
    watch_in(std::time::Duration::ZERO);
}

/// At the top of `main`: take the count out of the environment, so
/// that nothing this shell starts but its own heir has it.
pub(crate) fn take_count() {
    let n = std::env::var(RESEATS_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(RESEATS_ENV) };
    RESEATS.store(n, std::sync::atomic::Ordering::Release);
}

/// For the next image of this process: it is the same shell.
pub(crate) fn carry_count(cmd: &mut std::process::Command) {
    cmd.env(RESEATS_ENV, RESEATS.load(std::sync::atomic::Ordering::Acquire).to_string());
}

/// For the shell started in this one's place.
pub(crate) fn count_for_heir() -> (String, String) {
    (RESEATS_ENV.into(), (RESEATS.load(std::sync::atomic::Ordering::Acquire) + 1).to_string())
}

/// How long LaunchServices is given to see an exit this process saw.
const EXIT_SEEN_WITHIN: std::time::Duration = std::time::Duration::from_secs(3);

/// Shells started one after another before the state is left alone.
const RESEATS_MOST: u32 = 3;

/// How long a shell that was started for the coalition and refused it
/// waits before it looks once more.  `lsappinfo` stops listing some
/// dead records that still have a coalition; the one met was the
/// app's own core's, left by a shell from before this was kept, and
/// the core this shell starts in its first seconds takes that over.
const HIDDEN_HOLDER_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Sleep until `pid` is gone.
fn wait_for_exit(pid: i32) {
    // SAFETY: plain kqueue calls on a queue made and closed here; an
    // all-zero kevent is a valid one to fill in.
    unsafe {
        let kq = libc::kqueue();
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = pid as usize;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD;
        ev.fflags = libc::NOTE_EXIT;
        // one that is gone already fails to add
        if libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 {
            let mut got: libc::kevent = std::mem::zeroed();
            while libc::kevent(kq, std::ptr::null(), 0, &mut got, 1, std::ptr::null()) < 1 {}
        }
        libc::close(kq);
    }
}

/// With the program that had the coalition gone: see what its record
/// became, and act on it.  Returns the next pid to wait for, if the
/// coalition went to yet another program.
fn take_back(coalition: u64, bundle_id: &str, gone: i32) -> Option<i32> {
    let me = std::process::id() as i32;
    let reseats = RESEATS.load(std::sync::atomic::Ordering::Acquire);
    let deadline = std::time::Instant::now() + EXIT_SEEN_WITHIN;
    let mut waited = false;
    loop {
        match ls_records::holder_of(&ls_records::list(), coalition, me, bundle_id) {
            Holder::Us => return None,
            // not seen to have exited yet
            Holder::Other { pid, dead: false } if pid == gone && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Holder::Other { pid, dead: false } => return Some(pid),
            Holder::Other { pid, dead: true } => {
                lx_warn!(
                    "SHELL_COALITION_KEPT_BY_DEAD",
                    "the program that had the sessions' coalition exited and its record keeps it, until that program is run again",
                    coalition = coalition,
                    pid = pid
                );
                KEPT_BY_DEAD.store(true, std::sync::atomic::Ordering::Release);
                return None;
            }
            // this shell was started to be given a coalition listed
            // under nobody, and was not given it
            Holder::Nobody | Holder::OurDead if reseats > 0 && !waited => {
                KEPT_BY_DEAD.store(true, std::sync::atomic::Ordering::Release);
                std::thread::sleep(HIDDEN_HOLDER_WAIT);
                waited = true;
            }
            Holder::Nobody | Holder::OurDead if reseats >= RESEATS_MOST => {
                lx_warn!("SHELL_COALITION_NOT_TAKEN", "three shells in a row were not given the coalition; leaving it", coalition = coalition);
                return None;
            }
            Holder::Nobody | Holder::OurDead => {
                lx_event!(
                    "SHELL_COALITION_FREE",
                    "the sessions' coalition is nobody's: a new shell is started to be given it",
                    coalition = coalition,
                    was_under = gone
                );
                // does not return when the new shell answers
                ownership::start_heir_here(crate::resident::is_resident(), &ownership::kept_sessions());
                return None;
            }
        }
    }
}

/// Called once at every start of a shell, when it is the shell that
/// goes on: when this process is registered and its coalition is not
/// listed under it, get it back.  One look, on a thread of its own; a
/// shell that has its coalition, which is every shell but one that
/// lost it to a program started in a pane, ends there.
fn watch_in(delay: std::time::Duration) {
    if LOOKING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let (Some(coalition), Some(id)) = (platform::coalition_of(std::process::id() as i32), crate::identity::bundle_id()) else {
        return;
    };
    let look = move || {
        std::thread::sleep(delay);
        let me = std::process::id() as i32;
        let records = ls_records::list();
        if !records.iter().any(|r| r.pid == me && !r.dead) {
            return;
        }
        let mut gone = match ls_records::holder_of(&records, coalition, me, &id) {
            Holder::Us => return,
            Holder::Other { pid, dead: false } => {
                lx_event!(
                    "SHELL_COALITION_HELD_BY",
                    "the sessions' coalition is listed under another program; waiting for it to exit",
                    coalition = coalition,
                    pid = pid
                );
                wait_for_exit(pid);
                pid
            }
            Holder::Other { pid, dead: true } => pid,
            Holder::Nobody | Holder::OurDead => 0,
        };
        while let Some(next) = take_back(coalition, &id, gone) {
            wait_for_exit(next);
            gone = next;
        }
    };
    if let Err(e) = std::thread::Builder::new().name("l1-coalition".into()).spawn(look) {
        lx_warn!("shell.reclaim.thread_failed", &format!("{e}"));
    }
}

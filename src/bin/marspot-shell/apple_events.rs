//! Apple events after the shell replaces its own image.
//!
//! Apple events reach an app through appleeventsd, which keeps one
//! registration per process.  When a process's connection to it breaks,
//! appleeventsd unregisters "any application with that pid" -- by pid,
//! whichever connection the registration came in on.  The shell's
//! `execv` (an update in place, or a resident becoming the app again)
//! breaks the old image's connection, and appleeventsd takes the break
//! up only when something next connects to it.  Left alone, that next
//! thing is the new image's AppKit registering: the break and the
//! registration are then handled side by side on two of appleeventsd's
//! threads, and when the registration lands before the removal, the
//! removal takes it too.  The app gets no Apple events for as long as it
//! runs -- a quit from the Dock or from another app is refused.  On a
//! loaded runner that was 6 quits in 58.
//!
//! So an image started by such an exec asks appleeventsd one question
//! first thing in `main` (where pid 1's events would go: nowhere, it is
//! not an app).  That opens this image's connection, appleeventsd takes
//! the old one's break up then, and AppKit registers some 80 ms later.
//!
//! That narrows the race and does not close it: on a loaded machine
//! appleeventsd has taken 80-90 ms to finish a removal it had started,
//! and a registration arriving inside that is still lost.  When the
//! removal is finished cannot be asked -- an event a process sends its
//! own pid never leaves the process, and from outside a pid whose app is
//! about to be removed already answers "no such process" -- but whether
//! this image is registered can, once it is.  A child of this binary
//! ([`CHECK_ARG`]) asks whether it may send the shell an event
//! (`AEDeterminePermissionToAutomateTarget`, never asking the user).
//! "No such process" means the registration was lost, and the shell
//! replaces its image once more, at most [`RETRIES_MOST`] times; any
//! other answer is about permission, so there is an app to ask about.
//!
//! A lost registration refuses every quit from the Dock until the image
//! is replaced, and a person reaches for the Dock once the window is
//! back.  So the first look is taken as early as it means anything,
//! [`FIRST_LOOK`] after the question, once the event loop runs (AppKit
//! has registered by then), and the windows show no frame before it has
//! found the image registered ([`may_present`]): a window whose content
//! is up is one that can be quit.  With a single pane the first frame
//! was ready some 150 ms after the question; with a day's windows it
//! takes seconds, and the look costs nothing.  An answer of "no such
//! process" this early is asked once more before it is believed;
//! "registered" this early is looked at again at [`LAST_LOOK`], long
//! past any removal seen.
//! A resident (the app quit with Cmd-Q, which is no Apple event, while
//! unregistered) is replaced as a resident: it is opened again by an
//! Apple event, and would not be.
//!
//! The child asks and sends nothing.  Sending would register it: to send
//! to a live app, the sender checks in with appleeventsd itself, as an
//! app named marspot-shell, whether or not it wants a reply.  Asking only
//! looks the shell up.  The question and the looks happen once per exec;
//! a launch that did not come from one does neither.

mod child;
mod seam;

use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use marspot::app::EventProxy;

pub(crate) use child::check_main;

/// Set by an exec that replaces a shell AppKit had registered.
pub(crate) const REPLACED_ENV: &str = "MARSPOT_IMAGE_REPLACED";

/// How many times in a row the image was replaced because the check
/// found it unregistered.
const RETRIES_ENV: &str = "MARSPOT_AE_RETRIES";

/// Most replacements in a row for a lost registration.
const RETRIES_MOST: u32 = 2;

/// The child that checks: `--ae-check <pid>`.
pub(crate) const CHECK_ARG: &str = "--ae-check";

/// From the question's answer to the first look: three times the
/// longest removal seen (90 ms).
const FIRST_LOOK: Duration = Duration::from_millis(300);

/// An early "no such process" is asked again this much later: AppKit's
/// own registration can still be on its way in.
const LOOK_AGAIN: Duration = Duration::from_millis(200);

/// From the question's answer to the last look, after an early one
/// found the image registered.
const LAST_LOOK: Duration = Duration::from_secs(2);

/// From the first look to the frames being shown whatever the check is
/// doing: past two looks of [`child::CHILD_MOST`] and the pause between,
/// so only a step stuck in a way not foreseen gets here.
const PRESENT_MOST: Duration = Duration::from_millis(
    2 * child::CHILD_MOST.as_millis() as u64 + LOOK_AGAIN.as_millis() as u64 + 300,
);

/// Time the dev seam spent losing the registration before the first
/// look, in milliseconds: not counted against [`PRESENT_MOST`].
static SEAM_MS: AtomicU64 = AtomicU64::new(0);

/// This module's event class, and the event of the question.
const CLASS: u32 = u32::from_be_bytes(*b"MSPa");
const ASK_ID: u32 = u32::from_be_bytes(*b"exec");
const CHECK_ID: u32 = u32::from_be_bytes(*b"chek");

/// What the child exits with when the shell's pid has no app.
const CHECK_UNREGISTERED: i32 = 3;

/// Where this image is with the check: nothing to do, waiting for the
/// event loop, checking, or found unregistered.  Whoever moves it from
/// `LOST` replaces the image.
const IDLE: u8 = 0;
const AWAITING: u8 = 1;
const CHECKING: u8 = 2;
const LOST: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(IDLE);
static RETRIES: AtomicU32 = AtomicU32::new(0);
/// Cleared by an image from an exec until its first look found it
/// registered, or the looks are given up.
static PRESENT_OK: AtomicBool = AtomicBool::new(true);

/// What the question came back with, and how long it took in
/// microseconds; `i32::MIN` until it was asked.
static ASKED: AtomicI32 = AtomicI32::new(i32::MIN);
static TOOK_US: AtomicU64 = AtomicU64::new(0);
/// When the question came back: what the looks are timed from.
static ASKED_AT: OnceLock<Instant> = OnceLock::new();

/// Mark `cmd`, an exec of this shell, as replacing a registered image.
pub(crate) fn mark(cmd: &mut Command) {
    cmd.env(REPLACED_ENV, "1");
}

/// Mark `cmd`, an exec of this image, as the `n`th replacement for a
/// lost registration.
pub(crate) fn mark_retry(cmd: &mut Command, n: u32) {
    mark(cmd);
    cmd.env(RETRIES_ENV, n.to_string());
}

/// First thing in `main`, before any logging and before AppKit: when
/// this image replaced a registered one, have appleeventsd take the old
/// connection's break up now, and check the registration later.
/// Reported by [`report`].
pub(crate) fn settle_after_exec() {
    if std::env::var_os(REPLACED_ENV).is_none() {
        return;
    }
    let retries = std::env::var(RETRIES_ENV)
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    // SAFETY: the first thing main does, before any thread is started.
    unsafe {
        std::env::remove_var(REPLACED_ENV);
        std::env::remove_var(RETRIES_ENV);
    }
    RETRIES.store(retries, Ordering::Relaxed);
    STATE.store(AWAITING, Ordering::SeqCst);
    PRESENT_OK.store(false, Ordering::Relaxed);
    let started = Instant::now();
    let answer = child::ask_where_pid_one_goes();
    let _ = ASKED_AT.set(Instant::now());
    TOOK_US.store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    ASKED.store(answer, Ordering::Relaxed);
}

/// Log what [`settle_after_exec`] did, once the log is open.
pub(crate) fn report() {
    let answer = ASKED.load(Ordering::Relaxed);
    if answer == i32::MIN {
        return;
    }
    marspot::lx_event!(
        "SHELL_AE_SETTLED",
        "asked appleeventsd once, so the previous image's registration goes before this one registers",
        answer = answer,
        took_us = TOOK_US.load(Ordering::Relaxed),
        retries = RETRIES.load(Ordering::Relaxed)
    );
}

/// Whether a window may show its first frame: one load.
pub(crate) fn may_present() -> bool {
    PRESENT_OK.load(Ordering::Relaxed)
}

/// Let windows show their frames, waking the event loop that holds them.
pub(crate) fn let_present(wake: Option<&EventProxy>) {
    if !PRESENT_OK.swap(true, Ordering::Relaxed)
        && let Some(proxy) = wake
    {
        proxy.wake();
    }
}

/// Called on every supervisor tick, which runs inside the event loop:
/// one load, unless this image came from an exec and is not done with
/// its check.  Returns the number of the replacement to make when the
/// check found this image unregistered.
pub(crate) fn tick(proxy: &EventProxy) -> Option<u32> {
    match STATE.load(Ordering::SeqCst) {
        AWAITING => {
            if STATE
                .compare_exchange(AWAITING, CHECKING, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                start_check(Some(proxy.clone()));
            }
            None
        }
        LOST => take_lost(),
        _ => None,
    }
}

/// Called by the resident once it has said it is one: a check found
/// unregistered replaces it as a resident, and one not started yet
/// starts now.
pub(crate) fn as_resident() {
    if let Some(n) = take_lost() {
        replace_resident(n);
    } else if STATE
        .compare_exchange(AWAITING, CHECKING, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        start_check(None);
    }
}

/// Move the state from `LOST`, for the caller to replace the image.
fn take_lost() -> Option<u32> {
    STATE
        .compare_exchange(LOST, IDLE, Ordering::SeqCst, Ordering::SeqCst)
        .ok()?;
    let n = RETRIES.load(Ordering::Relaxed) + 1;
    if n > RETRIES_MOST {
        marspot::lx_error!(
            "shell.ae_check.gave_up",
            "not registered for Apple events after two replacements: a quit from the Dock is refused",
            retries = n - 1
        );
        let_present(None);
        return None;
    }
    Some(n)
}

/// Replace the resident with itself, as a resident.
fn replace_resident(n: u32) {
    marspot::lx_event!(
        "SHELL_AE_REPLACE",
        "replacing the resident so that it registers for Apple events",
        replacement = n,
        resident = true
    );
    let n = n.to_string();
    let extra = [
        (marspot_term::owner::RESIDENT_ENV, "1"),
        (RETRIES_ENV, n.as_str()),
    ];
    let Some(err) = crate::resident::replace_as_resident(&extra) else {
        return;
    };
    marspot::lx_error!(
        "shell.ae_check.exec_failed",
        &format!("{err}"),
        resident = true
    );
}

/// `wake` is the app's event loop; a resident has none to wake and is
/// replaced from here.
fn start_check(wake: Option<EventProxy>) {
    if wake.is_some() {
        let since = *ASKED_AT.get_or_init(Instant::now);
        let first_look = (since + FIRST_LOOK).max(Instant::now());
        hold_frames_at_most(first_look, PRESENT_MOST, wake.clone());
    }
    let spawned = std::thread::Builder::new()
        .name("ae-check".into())
        .spawn(move || {
            if !child::lost(wake.as_ref()) {
                STATE.store(IDLE, Ordering::SeqCst);
                return;
            }
            STATE.store(LOST, Ordering::SeqCst);
            if crate::resident::is_resident() {
                if let Some(n) = take_lost() {
                    replace_resident(n);
                }
            } else if let Some(proxy) = wake {
                proxy.wake();
            }
        });
    if let Err(e) = spawned {
        STATE.store(IDLE, Ordering::SeqCst);
        PRESENT_OK.store(true, Ordering::Relaxed);
        marspot::lx_warn!("shell.ae_check.not_run", &e.to_string());
    }
}

/// Show the frames `most` after `from`, plus what the dev seam took,
/// if the check has not let them be shown by then.
fn hold_frames_at_most(from: Instant, most: Duration, wake: Option<EventProxy>) {
    let spawned = std::thread::Builder::new()
        .name("ae-present-by".into())
        .spawn(move || {
            loop {
                let at = from + most + Duration::from_millis(SEAM_MS.load(Ordering::Relaxed));
                let now = Instant::now();
                if now >= at {
                    break;
                }
                std::thread::sleep(at - now);
            }
            if !PRESENT_OK.load(Ordering::Relaxed) {
                marspot::lx_warn!(
                    "shell.ae_check.present_deadline",
                    "the check did not finish its early looks in time: showing the frames anyway",
                    after_ms = from.elapsed().as_millis() as u64
                );
                let_present(wake.as_ref());
            }
        });
    if let Err(e) = spawned {
        marspot::lx_warn!("shell.ae_check.no_deadline", &e.to_string());
        let_present(None);
    }
}

#[cfg(test)]
mod tests;

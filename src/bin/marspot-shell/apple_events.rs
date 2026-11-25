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
//! this image is registered can, once it is.  A second after its first
//! frame is on screen, a child of this binary ([`CHECK_ARG`]) asks
//! whether it may send the shell an event
//! (`AEDeterminePermissionToAutomateTarget`, never asking the user).
//! "No such process" means the registration was lost, and the shell
//! replaces its image once more, at most [`RETRIES_MOST`] times; any
//! other answer is about permission, so there is an app to ask about.
//!
//! The child asks and sends nothing.  Sending would register it: to send
//! to a live app, the sender checks in with appleeventsd itself, as an
//! app named marspot-shell, whether or not it wants a reply.  Asking only
//! looks the shell up.  Both the question and the check happen once per
//! exec; a launch that did not come from one does neither.

use std::ffi::c_void;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use marspot::app::EventProxy;

/// Set by an exec that replaces a shell AppKit had registered.
pub(crate) const REPLACED_ENV: &str = "MARSPOT_IMAGE_REPLACED";

/// How many times in a row the image was replaced because the check
/// found it unregistered.
const RETRIES_ENV: &str = "MARSPOT_AE_RETRIES";

/// Most replacements in a row for a lost registration.
const RETRIES_MOST: u32 = 2;

/// The child that checks: `--ae-check <pid>`.
pub(crate) const CHECK_ARG: &str = "--ae-check";

/// From the first frame on screen to the check: well past the longest
/// removal seen (90 ms), so a registration found is one that stays.
const CHECK_AFTER: Duration = Duration::from_secs(1);

/// This module's event class, and the event of the question.
const CLASS: u32 = u32::from_be_bytes(*b"MSPa");
const ASK_ID: u32 = u32::from_be_bytes(*b"exec");
const CHECK_ID: u32 = u32::from_be_bytes(*b"chek");

const NO_REPLY: i32 = 1;
const PROC_NOT_FOUND: i32 = -600;

/// What the child exits with when the shell's pid has no app.
const CHECK_UNREGISTERED: i32 = 3;

/// Where this image is with the check: nothing to do, waiting for the
/// first frame, checking, or found unregistered.
const IDLE: u8 = 0;
const AWAITING: u8 = 1;
const CHECKING: u8 = 2;
const LOST: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(IDLE);
static RETRIES: AtomicU32 = AtomicU32::new(0);

/// What the question came back with, and how long it took in
/// microseconds; `i32::MIN` until it was asked.
static ASKED: AtomicI32 = AtomicI32::new(i32::MIN);
static TOOK_US: AtomicU64 = AtomicU64::new(0);

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
    let retries = std::env::var(RETRIES_ENV).ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    // SAFETY: the first thing main does, before any thread is started.
    unsafe {
        std::env::remove_var(REPLACED_ENV);
        std::env::remove_var(RETRIES_ENV);
    }
    RETRIES.store(retries, Ordering::Relaxed);
    STATE.store(AWAITING, Ordering::Relaxed);
    let started = Instant::now();
    let answer = ask_where_pid_one_goes();
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

/// Called on every supervisor tick: one load, unless this image came
/// from an exec and is not done with its check.  `on_screen` is whether
/// a window has shown a frame.  Returns the number of the replacement
/// to make when the check found this image unregistered.
pub(crate) fn tick(on_screen: impl FnOnce() -> bool, proxy: &EventProxy) -> Option<u32> {
    match STATE.load(Ordering::Relaxed) {
        IDLE | CHECKING => None,
        AWAITING => {
            if on_screen() {
                STATE.store(CHECKING, Ordering::Relaxed);
                start_check(proxy.clone());
            }
            None
        }
        _ => {
            STATE.store(IDLE, Ordering::Relaxed);
            let n = RETRIES.load(Ordering::Relaxed) + 1;
            if n > RETRIES_MOST {
                marspot::lx_error!(
                    "shell.ae_check.gave_up",
                    "not registered for Apple events after two replacements: a quit from the Dock is refused",
                    retries = n - 1
                );
                return None;
            }
            Some(n)
        }
    }
}

fn start_check(proxy: EventProxy) {
    let spawned = std::thread::Builder::new().name("ae-check".into()).spawn(move || {
        std::thread::sleep(CHECK_AFTER);
        match run_checker() {
            Ok((child, Some(CHECK_UNREGISTERED))) => {
                marspot::lx_warn!(
                    "shell.ae_check.lost",
                    "appleeventsd has no app at this pid: replacing the image again",
                    child = child,
                    retries = RETRIES.load(Ordering::Relaxed)
                );
                STATE.store(LOST, Ordering::Relaxed);
                proxy.wake();
            }
            Ok((child, code)) => {
                marspot::lx_event!(
                    "SHELL_AE_CHECKED",
                    "registered for Apple events after the exec",
                    child = child,
                    child_exit = code.unwrap_or(-1),
                    retries = RETRIES.load(Ordering::Relaxed)
                );
                STATE.store(IDLE, Ordering::Relaxed);
            }
            Err(e) => {
                marspot::lx_warn!("shell.ae_check.not_run", &e, retries = RETRIES.load(Ordering::Relaxed));
                STATE.store(IDLE, Ordering::Relaxed);
            }
        }
    });
    if let Err(e) = spawned {
        STATE.store(IDLE, Ordering::Relaxed);
        marspot::lx_warn!("shell.ae_check.not_run", &e.to_string());
    }
}

/// Start the child, wait for it, and return its pid and exit code.
fn run_checker() -> Result<(u32, Option<i32>), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg(CHECK_ARG)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let pid = child.id();
    let status = child.wait().map_err(|e| e.to_string())?;
    Ok((pid, status.code()))
}

/// The child: ask whether `pid` may be sent this module's check event,
/// without ever asking the user.  Exits [`CHECK_UNREGISTERED`] on "no
/// such process"; any other answer is about permission, so there is an
/// app at `pid` to ask about, and the exit code only says which answer
/// it was, for the log.
pub(crate) fn check_main(pid: Option<&str>) -> i32 {
    let Some(pid) = pid.and_then(|p| p.parse::<i32>().ok()) else { return 2 };
    let mut target = AEDesc::NULL;
    // SAFETY: plain C calls on a descriptor this function owns and
    // disposes of; the data pointer outlives the call that copies it.
    let answer = unsafe {
        let made = AECreateDesc(TYPE_KERNEL_PROCESS_ID, (&raw const pid).cast(), size_of::<i32>() as isize, &mut target);
        if made != 0 {
            return 2;
        }
        let answer = AEDeterminePermissionToAutomateTarget(&target, CLASS, CHECK_ID, 0);
        AEDisposeDesc(&mut target);
        answer
    };
    match answer {
        PROC_NOT_FOUND => CHECK_UNREGISTERED,
        0 => 0,
        // errAEEventWouldRequireUserConsent, errAEEventNotPermitted
        -1744 => 5,
        -1743 => 6,
        _ => 7,
    }
}

const TYPE_KERNEL_PROCESS_ID: u32 = u32::from_be_bytes(*b"kpid");

#[repr(C)]
struct AEDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

impl AEDesc {
    const NULL: Self = Self { descriptor_type: u32::from_be_bytes(*b"null"), data_handle: std::ptr::null_mut() };
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AECreateDesc(type_code: u32, data: *const c_void, size: isize, result: *mut AEDesc) -> i16;
    fn AECreateAppleEvent(
        class: u32,
        id: u32,
        target: *const AEDesc,
        return_id: i16,
        transaction_id: i32,
        result: *mut AEDesc,
    ) -> i16;
    fn AESendMessage(event: *const AEDesc, reply: *mut AEDesc, mode: i32, timeout_ticks: isize) -> i32;
    fn AEDisposeDesc(desc: *mut AEDesc) -> i16;
    fn AEDeterminePermissionToAutomateTarget(target: *const AEDesc, class: u32, id: u32, ask_user_if_needed: u8) -> i32;
}

/// Send pid 1 this module's question, with no reply asked for.  Pid 1
/// is not an app, so the answer is procNotFound (-600) and nothing is
/// sent; what matters is that appleeventsd was asked.
fn ask_where_pid_one_goes() -> i32 {
    const AUTO_RETURN_ID: i16 = -1;
    let pid: i32 = 1;
    let mut target = AEDesc::NULL;
    let mut event = AEDesc::NULL;
    let mut reply = AEDesc::NULL;
    // SAFETY: plain C calls on descriptors this function owns and
    // disposes of; the data pointer outlives the call that copies it.
    unsafe {
        let made = AECreateDesc(TYPE_KERNEL_PROCESS_ID, (&raw const pid).cast(), size_of::<i32>() as isize, &mut target);
        if made != 0 {
            return made as i32;
        }
        let made = AECreateAppleEvent(CLASS, ASK_ID, &target, AUTO_RETURN_ID, 0, &mut event);
        let answer = if made == 0 { AESendMessage(&event, &mut reply, NO_REPLY, 0) } else { made as i32 };
        AEDisposeDesc(&mut event);
        AEDisposeDesc(&mut target);
        AEDisposeDesc(&mut reply);
        answer
    }
}

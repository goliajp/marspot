//! The calls to appleeventsd: the question an image asks first thing
//! after an exec, and the looks a child of it takes at its registration.

use std::ffi::c_void;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use marspot::app::EventProxy;

use super::{
    ASK_ID, ASKED_AT, CHECK_ARG, CHECK_ID, CHECK_UNREGISTERED, CLASS, FIRST_LOOK, LAST_LOOK,
    LOOK_AGAIN, RETRIES, let_present,
};

const NO_REPLY: i32 = 1;

/// Longest a look's child is waited for: an appleeventsd or tccd that
/// never answers must not keep the check, and the frames, waiting.
pub(super) const CHILD_MOST: Duration = Duration::from_secs(1);

/// How often a running child is asked whether it has exited; a look
/// usually takes a few milliseconds.
const CHILD_POLL: Duration = Duration::from_millis(5);
const PROC_NOT_FOUND: i32 = -600;

/// The child: ask whether `pid` may be sent this module's check event,
/// without ever asking the user.  Exits [`CHECK_UNREGISTERED`] on "no
/// such process"; any other answer is about permission, so there is an
/// app at `pid` to ask about, and the exit code only says which answer
/// it was, for the log.
pub(crate) fn check_main(pid: Option<&str>) -> i32 {
    let Some(pid) = pid.and_then(|p| p.parse::<i32>().ok()) else {
        return 2;
    };
    let mut target = AEDesc::NULL;
    // SAFETY: plain C calls on a descriptor this function owns and
    // disposes of; the data pointer outlives the call that copies it.
    let answer = unsafe {
        let made = AECreateDesc(
            TYPE_KERNEL_PROCESS_ID,
            (&raw const pid).cast(),
            size_of::<i32>() as isize,
            &mut target,
        );
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
    const NULL: Self = Self {
        descriptor_type: u32::from_be_bytes(*b"null"),
        data_handle: std::ptr::null_mut(),
    };
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
    fn AESendMessage(
        event: *const AEDesc,
        reply: *mut AEDesc,
        mode: i32,
        timeout_ticks: isize,
    ) -> i32;
    fn AEDisposeDesc(desc: *mut AEDesc) -> i16;
    fn AEDeterminePermissionToAutomateTarget(
        target: *const AEDesc,
        class: u32,
        id: u32,
        ask_user_if_needed: u8,
    ) -> i32;
}

/// Send pid 1 this module's question, with no reply asked for.  Pid 1
/// is not an app, so the answer is procNotFound (-600) and nothing is
/// sent; what matters is that appleeventsd was asked.
pub(super) fn ask_where_pid_one_goes() -> i32 {
    const AUTO_RETURN_ID: i16 = -1;
    let pid: i32 = 1;
    let mut target = AEDesc::NULL;
    let mut event = AEDesc::NULL;
    let mut reply = AEDesc::NULL;
    // SAFETY: plain C calls on descriptors this function owns and
    // disposes of; the data pointer outlives the call that copies it.
    unsafe {
        let made = AECreateDesc(
            TYPE_KERNEL_PROCESS_ID,
            (&raw const pid).cast(),
            size_of::<i32>() as isize,
            &mut target,
        );
        if made != 0 {
            return made as i32;
        }
        let made = AECreateAppleEvent(CLASS, ASK_ID, &target, AUTO_RETURN_ID, 0, &mut event);
        let answer = if made == 0 {
            AESendMessage(&event, &mut reply, NO_REPLY, 0)
        } else {
            made as i32
        };
        AEDisposeDesc(&mut event);
        AEDisposeDesc(&mut target);
        AEDisposeDesc(&mut reply);
        answer
    }
}

/// The looks, each in a child; true when this image is not registered.
/// Frames may be shown once an early look found it registered.
pub(super) fn lost(wake: Option<&EventProxy>) -> bool {
    let since = *ASKED_AT.get_or_init(Instant::now);
    sleep_until(since + FIRST_LOOK);
    super::seam::drop_registration_if_armed(RETRIES.load(Ordering::Relaxed), "first");
    let mut unregistered = look(since, "first");
    if unregistered {
        std::thread::sleep(LOOK_AGAIN);
        unregistered = look(since, "again");
    }
    if !unregistered {
        let_present(wake);
        sleep_until(since + LAST_LOOK);
        super::seam::drop_registration_if_armed(RETRIES.load(Ordering::Relaxed), "last");
        unregistered = look(since, "last");
    }
    let retries = RETRIES.load(Ordering::Relaxed);
    let after_ms = since.elapsed().as_millis() as u64;
    if unregistered {
        marspot::lx_warn!(
            "shell.ae_check.lost",
            "appleeventsd has no app at this pid: replacing the image again",
            after_ms = after_ms,
            retries = retries
        );
    } else {
        marspot::lx_event!(
            "SHELL_AE_CHECKED",
            "registered for Apple events after the exec",
            after_ms = after_ms,
            retries = retries
        );
    }
    unregistered
}

fn sleep_until(at: Instant) {
    std::thread::sleep(at.saturating_duration_since(Instant::now()));
}

/// One look; true only on "no such process".  A child that did not run
/// is no answer, and counts as registered.
pub(super) fn look(since: Instant, which: &str) -> bool {
    let after_ms = since.elapsed().as_millis() as u64;
    match run_checker() {
        Ok((child, code)) => {
            let unregistered = code == Some(CHECK_UNREGISTERED);
            marspot::lx_event!(
                "SHELL_AE_LOOKED",
                if unregistered {
                    "no app at this pid"
                } else {
                    "registered"
                },
                look = which,
                after_ms = after_ms,
                child = child,
                child_exit = code.unwrap_or(-1)
            );
            unregistered
        }
        Err(e) => {
            marspot::lx_warn!("shell.ae_check.not_run", &e, look = which);
            false
        }
    }
}

/// Whether a child finds this image registered now; for the dev seam.
pub(super) fn registered_now() -> bool {
    run_checker().is_ok_and(|(_, code)| code != Some(CHECK_UNREGISTERED))
}

/// Start the child and return its pid and exit code, `None` when it was
/// killed for taking too long.
fn run_checker() -> Result<(u32, Option<i32>), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg(CHECK_ARG).arg(std::process::id().to_string());
    run_bounded(cmd, CHILD_MOST)
}

/// Start `cmd`, give it `most` to exit, and return its pid and exit
/// code.  One still running then is killed, by its pid, and reaped; it
/// has no code, which every caller takes as "registered".
pub(super) fn run_bounded(mut cmd: Command, most: Duration) -> Result<(u32, Option<i32>), String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let pid = child.id();
    let by = Instant::now() + most;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok((pid, status.code()));
        }
        if Instant::now() >= by {
            break;
        }
        std::thread::sleep(CHILD_POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
    marspot::lx_warn!(
        "shell.ae_check.child_stuck",
        "the check's child did not answer in time: killed, and the image taken as registered",
        child = pid,
        waited_ms = most.as_millis() as u64
    );
    Ok((pid, None))
}

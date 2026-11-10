//! Telling the scheduler that this thread is what the user is
//! looking at.
//!
//! Measured on an otherwise-idle bench host, one variable — CPU load
//! — swept by background hogs, 2000 frames each:
//!
//! | load | frame p99 | **GPU exec p99** | **GPU wait p99** |
//! |---|---:|---:|---:|
//! | idle | 293 µs | 111 µs | 279 µs |
//! | 14 hogs | 7,453 µs | **117 µs** | **7,425 µs** |
//! | 42 hogs | 1,720 µs | **115 µs** | 1,546 µs |
//!
//! The GPU's own account of the frame does not move — 111 µs idle,
//! 117 µs under load.  The *wall clock* around the same wait grows
//! 27×.  Nothing about the work got heavier; the thread waiting for
//! it stopped being run.  While it is not running, every pane is
//! frozen and the supervisor's PONG deadline is ticking, which is how
//! a busy machine used to end in "Marspot stopped — please restart
//! the app".
//!
//! macOS schedules by quality-of-service class, and a process spawned
//! from a shell gets an unspecified/default one — the same tier as
//! the batch work it is competing with.  `USER_INTERACTIVE` is the
//! tier for "a person is waiting on this frame", which is literally
//! what a terminal's render loop is.
//!
//! Applied to the *calling thread*, not the process: only the loop
//! that paints and pumps wants this priority.  Worker threads that
//! read transcripts or sweep state deliberately keep the default —
//! promoting everything promotes nothing.

/// `TASK_CATEGORY_POLICY` and `TASK_DEFAULT_APPLICATION` from
/// `<mach/task_policy.h>`.
const TASK_CATEGORY_POLICY: u32 = 1;
const TASK_DEFAULT_APPLICATION: i32 = 7;

/// `QOS_CLASS_USER_INTERACTIVE` from `<sys/qos.h>`.
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    /// Set the calling thread's QoS class.  Returns 0 on success.
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    /// `<mach/task.h>`; `policy` points at `count` integers.
    fn task_policy_set(task: libc::mach_port_t, flavor: u32, policy: *const i32, count: u32) -> i32;
}

/// Run this process as an application in the scheduler's eyes.
///
/// A thread's QoS is capped by its task's role.  A process the shell
/// spawns has no role, and asking for `USER_INTERACTIVE` there is
/// accepted and then held at the default tier: priority 31, the same
/// as a compiler, where the shell's own main thread runs at 46.  On a
/// busy machine core's loop sat runnable for 60 ms at a time without
/// being given a core (2026-11-10).  As an application the same request
/// lands at 46.
///
/// For core only: the shell's role belongs to the system, which tracks
/// it as the app in front, and must not be overwritten.
// libc marks its Mach bindings deprecated in favour of the mach2
// crate; one call does not earn a dependency
#[allow(deprecated)]
pub fn take_application_role() -> bool {
    let role = TASK_DEFAULT_APPLICATION;
    // SAFETY: TASK_CATEGORY_POLICY reads one integer, the role, from
    // `policy`; the task is this one.
    let kr = unsafe { task_policy_set(libc::mach_task_self(), TASK_CATEGORY_POLICY, &role, 1) };
    kr == libc::KERN_SUCCESS
}

/// Ask the scheduler to treat this thread as user-interactive.
///
/// Returns whether it took.  A failure is worth logging and worth
/// carrying on from: the loop still runs, it just competes on equal
/// terms with whatever else the machine is doing.
pub fn raise_current_thread_to_user_interactive() -> bool {
    // `relative_priority` must be <= 0; 0 is the top of the class.
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    rc == 0
}

/// The scheduled priority of the calling thread, as the kernel has it.
#[cfg(test)]
fn current_thread_priority() -> i32 {
    // SAFETY: fills at most `count` integers of a thread_extended_info,
    // the struct THREAD_EXTENDED_INFO names.
    let mut info: libc::thread_extended_info = unsafe { std::mem::zeroed() };
    let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
    let kr = unsafe {
        libc::thread_info(
            libc::pthread_mach_thread_np(libc::pthread_self()),
            libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
            &mut info as *mut _ as libc::thread_info_t,
            &mut count,
        )
    };
    assert_eq!(kr, libc::KERN_SUCCESS);
    info.pth_priority
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The role is accepted and never lowers the thread.  How far it
    /// raises it depends on where the process sits: under the app it
    /// takes the loop from 31 to 46, while a process started over ssh
    /// is held at 37 either way, so the rise itself is checked on the
    /// installed app, not here.  Run in a child process, since the role
    /// is the whole process's.
    #[test]
    fn as_an_application_the_request_is_honoured() {
        if std::env::var_os("MARSPOT_QOS_CHILD").is_some() {
            assert!(raise_current_thread_to_user_interactive());
            let without = current_thread_priority();
            assert!(take_application_role());
            assert!(raise_current_thread_to_user_interactive());
            let with = current_thread_priority();
            println!("without={without} with={with}");
            assert!(with >= without, "without={without} with={with}");
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "qos::tests::as_an_application_the_request_is_honoured", "--nocapture"])
            .env("MARSPOT_QOS_CHILD", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("with="), "the child ran no test: {text}");
    }

    /// It has to actually succeed on the platform we ship to — a
    /// silent no-op would look exactly like a working call, and the
    /// only symptom would be a tail latency nobody could attribute.
    #[test]
    fn the_scheduler_accepts_the_request() {
        assert!(
            raise_current_thread_to_user_interactive(),
            "pthread_set_qos_class_self_np refused USER_INTERACTIVE",
        );
    }
}

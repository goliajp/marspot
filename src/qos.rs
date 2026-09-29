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

/// `QOS_CLASS_USER_INTERACTIVE` from `<sys/qos.h>`.
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

unsafe extern "C" {
    /// Set the calling thread's QoS class.  Returns 0 on success.
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
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

#[cfg(test)]
mod tests {
    use super::*;

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

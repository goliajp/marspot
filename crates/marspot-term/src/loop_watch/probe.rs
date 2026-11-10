//! What the loop's thread was doing while it stalled, read from the
//! watchdog thread so the loop itself pays nothing.
//!
//! A phase name says where the time went, not why: the same 150 ms in
//! a memcpy can be the loop computing, the loop runnable but not given
//! a core, or the loop asleep in the kernel on a page or a lock.  The
//! scheduler knows which, and these are its answers.

use std::time::Duration;

/// One look at the loop's thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Look {
    /// User plus system CPU time the thread has used, ever.
    pub cpu: Duration,
    pub state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// On a core or waiting for one.
    Runnable,
    /// Asleep in an interruptible wait: a lock, a pipe, a socket.
    Waiting,
    /// Asleep where it cannot be interrupted: disk, or a page fault.
    Uninterruptible,
    Other,
}

/// Process-wide counters a stall is compared against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counts {
    /// Page faults that had to read from disk.
    pub major_faults: i64,
    /// Times a thread of this process was preempted.
    pub preempted: i64,
}

impl Counts {
    pub fn now() -> Self {
        // SAFETY: getrusage writes one rusage into the zeroed struct
        // and reads nothing else.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
            return Self::default();
        }
        Self { major_faults: ru.ru_majflt as i64, preempted: ru.ru_nivcsw as i64 }
    }
}

/// A handle on the thread that created it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Probe {
    #[cfg(target_os = "macos")]
    port: libc::mach_port_t,
}

impl Probe {
    /// The calling thread.
    pub fn this_thread() -> Self {
        Self {
            // SAFETY: names the calling thread, which outlives every
            // use: the watchdog stops once the LoopWatch is dropped,
            // and the LoopWatch lives on this thread.
            #[cfg(target_os = "macos")]
            port: unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) },
        }
    }

    #[cfg(target_os = "macos")]
    pub fn look(&self) -> Option<Look> {
        // SAFETY: thread_info fills at most `count` integers of `info`,
        // which is a thread_basic_info, the struct THREAD_BASIC_INFO
        // names.
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        let kr = unsafe {
            libc::thread_info(
                self.port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                &mut info as *mut _ as libc::thread_info_t,
                &mut count,
            )
        };
        if kr != libc::KERN_SUCCESS {
            return None;
        }
        let t = |v: libc::time_value_t| {
            Duration::from_secs(v.seconds as u64) + Duration::from_micros(v.microseconds as u64)
        };
        let state = match info.run_state {
            libc::TH_STATE_RUNNING => State::Runnable,
            libc::TH_STATE_WAITING => State::Waiting,
            libc::TH_STATE_UNINTERRUPTIBLE => State::Uninterruptible,
            _ => State::Other,
        };
        Some(Look { cpu: t(info.user_time) + t(info.system_time), state })
    }

    #[cfg(not(target_os = "macos"))]
    pub fn look(&self) -> Option<Look> {
        None
    }
}

/// What the loop's thread did between the alarm and the end of the
/// stall.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    /// From the alarm to the iteration's end, as the watchdog saw it.
    pub window: Duration,
    /// CPU the loop's thread used inside `window`.
    pub cpu: Duration,
    /// Looks that found it runnable, waiting, uninterruptible, other.
    pub runnable: u32,
    pub waiting: u32,
    pub uninterruptible: u32,
    pub other: u32,
    /// Process-wide, inside `window`.
    pub major_faults: i64,
    pub preempted: i64,
}

impl Trace {
    pub(super) fn count(&mut self, state: State) {
        match state {
            State::Runnable => self.runnable += 1,
            State::Waiting => self.waiting += 1,
            State::Uninterruptible => self.uninterruptible += 1,
            State::Other => self.other += 1,
        }
    }

    /// One line for the log.
    pub fn summary(&self) -> String {
        format!(
            "after the alarm the loop thread used {} of CPU in {}; looks: {} runnable, {} waiting, \
             {} in uninterruptible wait, {} other; process: {} page-ins, {} preemptions",
            super::fmt_dur(self.cpu),
            super::fmt_dur(self.window),
            self.runnable,
            self.waiting,
            self.uninterruptible,
            self.other,
            self.major_faults,
            self.preempted,
        )
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn a_busy_thread_reads_as_runnable_and_burning_cpu() {
        let p = Probe::this_thread();
        let before = p.look().unwrap();
        let t = std::time::Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(30) {
            x = x.wrapping_mul(31).wrapping_add(1);
        }
        std::hint::black_box(x);
        let after = p.look().unwrap();
        assert_eq!(after.state, State::Runnable);
        assert!(after.cpu - before.cpu >= Duration::from_millis(20), "{before:?} → {after:?}");
    }

    #[test]
    fn a_sleeping_thread_reads_as_waiting_and_idle() {
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            tx.send(Probe::this_thread()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let p = rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let a = p.look().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let b = p.look().unwrap();
        assert_eq!(b.state, State::Waiting);
        assert!(b.cpu - a.cpu < Duration::from_millis(5), "{a:?} → {b:?}");
        h.join().unwrap();
    }
}

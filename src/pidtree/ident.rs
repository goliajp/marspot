//! Process identity, and the one way to signal a process.
//!
//! A pid alone names whatever process holds that number right now.
//! Pids are recycled, so anything remembered across a scan, a file or
//! a sleep must carry the start time too: the pair is what names one
//! process for its whole life (exec keeps both).

use std::fmt;

/// One process for its whole life: pid plus start time in
/// microseconds since the epoch (`pbi_start_tvsec`, `pbi_start_tvusec`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcId {
    pub pid: i32,
    pub start_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalError {
    /// No process has this pid, or the one that has it is not the
    /// process this id named: nothing was signalled.
    Gone,
    /// The process exists and is the one named, but we may not signal it.
    Denied,
    /// Any other errno from the lookup or from `kill`.
    Os(i32),
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalError::Gone => f.write_str("process is gone"),
            SignalError::Denied => f.write_str("not permitted to signal the process"),
            SignalError::Os(e) => write!(f, "{}", std::io::Error::from_raw_os_error(*e)),
        }
    }
}

impl std::error::Error for SignalError {}

pub(super) fn start_us_of(info: &libc::proc_bsdinfo) -> u64 {
    info.pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec)
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)`, with the errno on failure.
pub(super) fn bsdinfo(pid: i32) -> Result<libc::proc_bsdinfo, i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let r = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if r == size {
        return Ok(info);
    }
    if r > 0 {
        return Err(libc::EIO);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(0) | None => Err(libc::ESRCH),
        Some(e) => Err(e),
    }
}

/// The identity of the process that holds `pid` now.  None when no
/// process does, or the kernel will not say.
pub fn proc_id(pid: i32) -> Option<ProcId> {
    if pid <= 0 {
        return None;
    }
    let info = bsdinfo(pid).ok()?;
    Some(ProcId {
        pid,
        start_us: start_us_of(&info),
    })
}

/// Send `sig` to the process `id` names, and to no other.
///
/// Refuses pid 0, 1 and negative pids (group and broadcast forms).
pub fn signal(id: ProcId, sig: i32) -> Result<(), SignalError> {
    if id.pid <= 1 {
        return Err(SignalError::Os(libc::EINVAL));
    }
    match bsdinfo(id.pid) {
        Ok(info) if start_us_of(&info) == id.start_us => {}
        Ok(_) => return Err(SignalError::Gone),
        Err(libc::ESRCH) => return Err(SignalError::Gone),
        Err(libc::EPERM) => return Err(SignalError::Denied),
        Err(e) => return Err(SignalError::Os(e)),
    }
    // the pid can still be reaped and reused between the check above and this kill
    if unsafe { libc::kill(id.pid, sig) } == 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Err(SignalError::Gone),
        Some(libc::EPERM) => Err(SignalError::Denied),
        Some(e) => Err(SignalError::Os(e)),
        None => Err(SignalError::Os(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    fn sleeper() -> crate::test_env::ChildGuard {
        crate::test_env::ChildGuard::new(
            Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .expect("spawn /bin/sleep"),
        )
    }

    /// `try_wait` until the child exits, bounded.
    fn exited_within(child: &mut Child, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if child.try_wait().expect("try_wait").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn proc_id_of_self_is_stable_and_has_a_start_time() {
        let me = std::process::id() as i32;
        let a = proc_id(me).expect("own proc id");
        let b = proc_id(me).expect("own proc id");
        assert_eq!(a, b);
        assert!(
            a.start_us > 1_500_000_000_000_000,
            "start time {a:?} is not a recent epoch"
        );
        assert_eq!(proc_id(0), None);
        assert_eq!(proc_id(-1), None);
        assert_eq!(proc_id(i32::MAX), None);
    }

    #[test]
    fn signal_reaches_the_process_its_id_names() {
        let mut child = sleeper();
        let id = proc_id(child.id() as i32).expect("child proc id");
        assert_eq!(signal(id, libc::SIGTERM), Ok(()));
        assert!(
            exited_within(&mut child, Duration::from_secs(5)),
            "SIGTERM did not end the child"
        );
    }

    /// A start time that differs by one microsecond is a different
    /// process, which is what a reused pid looks like.
    #[test]
    fn signal_refuses_a_pid_that_now_names_another_process() {
        let mut child = sleeper();
        let real = proc_id(child.id() as i32).expect("child proc id");
        let stale = ProcId {
            pid: real.pid,
            start_us: real.start_us + 1,
        };
        assert_eq!(signal(stale, libc::SIGKILL), Err(SignalError::Gone));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the refused signal was delivered"
        );
        assert_eq!(
            proc_id(real.pid),
            Some(real),
            "the child must still be the same process"
        );
        signal(real, libc::SIGKILL).expect("kill the real child");
        assert!(exited_within(&mut child, Duration::from_secs(5)));
    }

    #[test]
    fn signal_after_exit_is_gone_and_low_pids_are_refused() {
        let mut child = sleeper();
        let id = proc_id(child.id() as i32).expect("child proc id");
        child.kill().expect("kill");
        child.wait().expect("reap");
        assert_eq!(signal(id, 0), Err(SignalError::Gone));
        for pid in [-1, 0, 1] {
            let r = signal(ProcId { pid, start_us: 0 }, 0);
            assert_eq!(r, Err(SignalError::Os(libc::EINVAL)), "pid {pid}");
        }
    }
}

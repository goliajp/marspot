//! The three things this crate asks the operating system that are not
//! the same call on every one.
//!
//! Everything else here is POSIX and compiles anywhere. These three
//! were written straight against Darwin's libc, which is why the
//! "zero-GUI engine" -- the crate `marspot-session` is built from, and
//! the one a second platform would reuse whole -- did not build on
//! Linux at all.
//!
//! Each is a portable question with a per-platform answer:
//!
//! | question | macOS | Linux |
//! |---|---|---|
//! | what executable is this pid running? | `proc_pidpath` | `/proc/<pid>/exe` |
//! | what pids exist? | `proc_listallpids` | the numeric entries of `/proc` |
//! | swap the environment before exec | `*_NSGetEnviron() = p` | `environ = p` |
//!
//! The failure modes have to match, not just the happy paths, because
//! one of the callers signals processes based on the answer. See
//! [`pid_exe_name`].

/// The file name of the executable `pid` is running, or `None`.
///
/// `None` means "could not establish it", and every caller must treat
/// that as "not one of ours". That is load-bearing:
/// `session_registry::pid_is_live_session` refuses to signal a pid
/// whose executable it cannot confirm, because a recycled pid that
/// looked live once got an innocent process killed.
///
/// Both platforms fail that way for the same reasons -- the process
/// exited between the check and the call, or we are not allowed to
/// ask. On Linux reading another user's `/proc/<pid>/exe` gives
/// EACCES, which is exactly the macOS sandbox case: being refused is
/// itself evidence the pid is not our child.
#[cfg(target_os = "macos")]
pub fn pid_exe_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    let path = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
    Some(path.rsplit('/').next().unwrap_or("").to_string())
}

#[cfg(target_os = "linux")]
pub fn pid_exe_name(pid: i32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(path.file_name()?.to_string_lossy().into_owned())
}

/// Every pid on the machine, or `None` when the question could not be
/// answered.
///
/// `None` is not "no processes" -- the one caller reads it as "somebody
/// might be alive" and defers, so the two must stay distinguishable.
#[cfg(target_os = "macos")]
pub fn all_pids() -> Option<Vec<i32>> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return None;
    }
    // Twice the reported count: the table can grow between the sizing
    // call and the filling one.
    let mut pids = vec![0i32; n as usize * 2];
    let filled = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if filled <= 0 {
        return None;
    }
    // `proc_listallpids` returns BYTES written, not entries -- the old
    // call site took `filled` as a count and read past the end into
    // zeros, which only went unnoticed because the loop below skipped
    // non-positive pids. Converting here means the list this returns
    // is the list, and a caller cannot inherit that mistake.
    pids.truncate(filled as usize / std::mem::size_of::<i32>());
    Some(pids)
}

#[cfg(target_os = "linux")]
pub fn all_pids() -> Option<Vec<i32>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        if let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
            out.push(pid);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Point `environ` at `ptrs` in the child between fork and exec.
///
/// # Safety
///
/// Call only between `fork` and `exec` in the child, with `ptrs` a
/// NULL-terminated array that outlives the call. A single pointer
/// store, which is what makes it async-signal-safe -- the array itself
/// must already be built, before the fork.
#[cfg(target_os = "macos")]
pub unsafe fn set_environ(ptrs: *mut *mut libc::c_char) {
    unsafe { *libc::_NSGetEnviron() = ptrs };
}

#[cfg(target_os = "linux")]
pub unsafe fn set_environ(ptrs: *mut *mut libc::c_char) {
    unsafe extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }
    unsafe { environ = ptrs };
}

/// What the tests ask the operating system.
///
/// These are the same shape of question as the three above -- portable
/// to ask, per-platform to answer -- but only the tests ask them, so
/// they are not part of the crate's surface. They live here rather
/// than beside each test because `current_rss_bytes` was written twice
/// already, in `pty.rs` and `terminal.rs`, and two copies of a
/// measurement drift.
#[cfg(test)]
pub mod testing {
    /// Live resident set size in bytes.
    ///
    /// Live, not peak: the soak tests assert a process that grew and
    /// stayed grown is caught, which `getrusage`'s `ru_maxrss` cannot
    /// say.
    #[cfg(target_os = "macos")]
    pub fn current_rss_bytes() -> u64 {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::proc_taskinfo>() as i32,
            )
        };
        if r <= 0 { 0 } else { info.pti_resident_size }
    }

    #[cfg(target_os = "linux")]
    pub fn current_rss_bytes() -> u64 {
        // statm's second field is resident pages.
        let s = match std::fs::read_to_string("/proc/self/statm") {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let pages: u64 = match s.split_whitespace().nth(1).and_then(|f| f.parse().ok()) {
            Some(p) => p,
            None => return 0,
        };
        pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64
    }

    /// How many file descriptors this process holds.
    ///
    /// Returns `None` rather than 0 when it cannot be counted: a leak
    /// test whose counter silently reads zero passes forever, which is
    /// worse than one that fails.
    pub fn count_open_fds() -> Option<usize> {
        #[cfg(target_os = "macos")]
        let dir = "/dev/fd";
        #[cfg(target_os = "linux")]
        let dir = "/proc/self/fd";
        std::fs::read_dir(dir).ok().map(|d| d.count())
    }

    /// Does `pid`'s terminal have a foreground job other than the
    /// shell itself?  Compares the tty's foreground process group with
    /// the process's own.
    #[cfg(target_os = "macos")]
    pub fn has_foreground_job(pid: i32) -> Option<bool> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::proc_bsdinfo>() as i32,
            )
        } > 0;
        if !ok {
            return None;
        }
        Some(info.e_tpgid as i32 != info.pbi_pgid as i32)
    }

    #[cfg(target_os = "linux")]
    pub fn has_foreground_job(pid: i32) -> Option<bool> {
        // /proc/<pid>/stat: field 5 is pgrp, field 8 is tpgid, both
        // counted after the comm field -- which can itself contain
        // spaces and parentheses, so it is cut at the LAST ')'.
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &s[s.rfind(')')? + 1..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        // rest[0] is state, so pgrp is rest[2] and tpgid is rest[5].
        let pgrp: i32 = f.get(2)?.parse().ok()?;
        let tpgid: i32 = f.get(5)?.parse().ok()?;
        Some(tpgid != pgrp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instrument has to be able to fail, so this asks about a pid
    /// whose answer is known two ways: our own, which must resolve,
    /// and one that cannot exist, which must not.
    #[test]
    fn a_live_pid_resolves_and_an_impossible_one_does_not() {
        let me = std::process::id() as i32;
        let name = pid_exe_name(me).expect("our own pid has an executable");
        assert!(!name.is_empty());
        assert!(!name.contains('/'), "a file name, not a path: {name:?}");
        // The test binary's name, whatever the harness calls it.
        assert!(
            name.starts_with("marspot_term") || name.starts_with("marspot-term"),
            "unexpected name for the test binary: {name:?}"
        );
        // Above the system maximum, so it is not merely unused.
        assert_eq!(pid_exe_name(i32::MAX), None, "an impossible pid");
    }

    #[test]
    fn the_pid_list_contains_us_and_is_not_absurd() {
        let pids = all_pids().expect("a machine has processes");
        let me = std::process::id() as i32;
        assert!(pids.contains(&me), "we are not in the list of running pids");
        assert!(pids.len() > 5, "only {} pids on a running machine?", pids.len());
        assert!(pids.iter().all(|&p| p > 0), "a pid must be positive");
    }
}

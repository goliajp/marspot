//! macOS process inspection — minimal FFI glue for "list every pid",
//! "what's this pid's cwd / cmdline / parent".  Used by the
//! claudecode plugin to walk down from a shelld session's child_pid
//! (the zsh at the top of the PTY) into the descendant `node` /
//! `claude` process and pull its working directory.
//!
//! Why hand-roll vs. crate: per the project's self-build principle,
//! this is one BSD sysctl + one proc_pidinfo + one KERN_PROCARGS2 —
//! a libc-only ~150 lines, not worth pulling in a deps tree.
//!
//! All public functions are best-effort: on error we return None or
//! an empty vec.  Plugin tick is on the host-budget-policed path —
//! must not panic.

use std::ffi::CStr;
use std::path::PathBuf;

/// One process as returned by the sysctl table.  We only keep what
/// the plugin actually needs.
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub pid: i32,
    pub ppid: i32,
    /// `kp_proc.p_comm` — first 16 bytes of the executable name, NUL-
    /// trimmed.  Cheap; enough for "is this `node`?".
    pub comm: String,
}

/// Snapshot of every running process on the host (kernel-level scan
/// via `sysctl(KERN_PROC, KERN_PROC_ALL)`).  Equivalent to walking
/// `ps -A` without spawning a process.  Typically 300–600 rows on
/// a dev machine; the table is materialised into a Vec we own.
pub fn list_all_procs() -> Vec<ProcRow> {
    unsafe { list_all_procs_inner().unwrap_or_default() }
}

unsafe fn list_all_procs_inner() -> Option<Vec<ProcRow>> {
    // proc_listpids(PROC_ALL_PIDS) — get every pid on the host.
    // libc crate doesn't expose PROC_ALL_PIDS; <sys/proc_info.h>
    // defines it as 1.
    const PROC_ALL_PIDS: u32 = 1;
    // Round 1: ask for size (pass NULL buf).
    let cap_bytes = libc::proc_listpids(
        PROC_ALL_PIDS,
        0,
        std::ptr::null_mut(),
        0,
    );
    if cap_bytes <= 0 {
        return None;
    }
    // Add slop for new forks between sizing and read.
    let extra = 64 * std::mem::size_of::<libc::pid_t>() as i32;
    let mut pids: Vec<libc::pid_t> =
        vec![0; ((cap_bytes + extra) as usize) / std::mem::size_of::<libc::pid_t>()];
    let got_bytes = libc::proc_listpids(
        PROC_ALL_PIDS,
        0,
        pids.as_mut_ptr() as *mut libc::c_void,
        (pids.len() * std::mem::size_of::<libc::pid_t>()) as i32,
    );
    if got_bytes <= 0 {
        return None;
    }
    let count = got_bytes as usize / std::mem::size_of::<libc::pid_t>();
    pids.truncate(count);
    let mut out = Vec::with_capacity(count);
    for pid in pids {
        if pid == 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let r = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        );
        if r <= 0 {
            continue; // process exited between listpids and pidinfo
        }
        let comm = CStr::from_ptr(info.pbi_comm.as_ptr())
            .to_string_lossy()
            .into_owned();
        out.push(ProcRow {
            pid,
            ppid: info.pbi_ppid as i32,
            comm,
        });
    }
    Some(out)
}

/// Working directory of `pid` via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
/// Returns None when the process has exited, when SIP / sandbox blocks
/// the call, or when the cwd isn't a regular path.
pub fn proc_cwd(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_vnodepathinfo>() as i32,
        )
    };
    if r <= 0 {
        return None;
    }
    // pvi_cdir.vip_path is NUL-terminated.  libc on some toolchains
    // declares it as a 2-D array (32×32 = 1024 bytes), so cast through
    // a raw byte pointer rather than `.as_ptr()` (which inherits the
    // inner array element type and fails the CStr signature).
    let path_buf = &info.pvi_cdir.vip_path;
    let cstr = unsafe { CStr::from_ptr(path_buf.as_ptr() as *const libc::c_char) };
    let bytes = cstr.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// Full command line of `pid` via `sysctl(KERN_PROCARGS2)`.  Returns
/// the argv joined by spaces, or None on failure.  Empty argv → empty
/// string.
pub fn proc_cmdline(pid: i32) -> Option<String> {
    unsafe { proc_cmdline_inner(pid) }
}

unsafe fn proc_cmdline_inner(pid: i32) -> Option<String> {
    // First read kern.argmax to size the buffer right.
    let mut argmax: libc::c_int = 0;
    let mut sz: libc::size_t = std::mem::size_of::<libc::c_int>();
    let mut mib_argmax: [libc::c_int; 2] = [libc::CTL_KERN, libc::KERN_ARGMAX];
    if libc::sysctl(
        mib_argmax.as_mut_ptr(),
        2,
        &mut argmax as *mut _ as *mut libc::c_void,
        &mut sz,
        std::ptr::null_mut(),
        0,
    ) != 0
    {
        return None;
    }
    if argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    // KERN_PROCARGS2 = 49.  libc crate may not expose it on all
    // versions, so use the numeric value directly.
    const KERN_PROCARGS2: libc::c_int = 49;
    let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, KERN_PROCARGS2, pid];
    let mut got: libc::size_t = buf.len();
    if libc::sysctl(
        mib.as_mut_ptr(),
        3,
        buf.as_mut_ptr() as *mut libc::c_void,
        &mut got,
        std::ptr::null_mut(),
        0,
    ) != 0
    {
        return None;
    }
    buf.truncate(got);
    // Layout: [argc: i32 LE][argv0_path: cstr][\0...padding...]
    //         [argv[0]: cstr][argv[1]: cstr]...[argv[argc-1]: cstr]
    //         [envp...]
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?);
    if argc < 0 {
        return None;
    }
    // Skip the i32 + the argv0_path c-string + its NUL padding.
    let mut cur = 4usize;
    // Walk past argv0_path (first NUL).
    while cur < buf.len() && buf[cur] != 0 {
        cur += 1;
    }
    // Skip the padding NULs to reach argv[0].
    while cur < buf.len() && buf[cur] == 0 {
        cur += 1;
    }
    // Now read `argc` NUL-separated strings.
    let mut parts: Vec<String> = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let start = cur;
        while cur < buf.len() && buf[cur] != 0 {
            cur += 1;
        }
        if start == cur {
            break;
        }
        let s = String::from_utf8_lossy(&buf[start..cur]).into_owned();
        parts.push(s);
        if cur < buf.len() {
            cur += 1; // skip the NUL
        }
    }
    Some(parts.join(" "))
}

/// Look up one environment variable on a running pid by walking
/// KERN_PROCARGS2 past argv into envp.  Returns the value of the
/// first match, or None when the var isn't set / the call is denied
/// (sandbox / SIP).
pub fn proc_env_value(pid: i32, key: &str) -> Option<String> {
    unsafe { proc_env_value_inner(pid, key) }
}

unsafe fn proc_env_value_inner(pid: i32, key: &str) -> Option<String> {
    // Same buffer-sizing dance as `proc_cmdline_inner`.
    let mut argmax: libc::c_int = 0;
    let mut sz: libc::size_t = std::mem::size_of::<libc::c_int>();
    let mut mib_argmax: [libc::c_int; 2] = [libc::CTL_KERN, libc::KERN_ARGMAX];
    if libc::sysctl(
        mib_argmax.as_mut_ptr(),
        2,
        &mut argmax as *mut _ as *mut libc::c_void,
        &mut sz,
        std::ptr::null_mut(),
        0,
    ) != 0
    {
        return None;
    }
    if argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    const KERN_PROCARGS2: libc::c_int = 49;
    let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, KERN_PROCARGS2, pid];
    let mut got: libc::size_t = buf.len();
    if libc::sysctl(
        mib.as_mut_ptr(),
        3,
        buf.as_mut_ptr() as *mut libc::c_void,
        &mut got,
        std::ptr::null_mut(),
        0,
    ) != 0
    {
        return None;
    }
    buf.truncate(got);
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[0..4].try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let mut cur = 4usize;
    // Skip argv0_path (a single c-string).
    while cur < buf.len() && buf[cur] != 0 {
        cur += 1;
    }
    // Skip padding NULs to argv[0].
    while cur < buf.len() && buf[cur] == 0 {
        cur += 1;
    }
    // Step over `argc` argv strings.
    for _ in 0..argc {
        while cur < buf.len() && buf[cur] != 0 {
            cur += 1;
        }
        if cur < buf.len() {
            cur += 1; // skip NUL
        }
    }
    // Now `cur` is at envp[0].  Walk NUL-separated KEY=VALUE entries
    // until we either find our key or hit a zero-length entry (envp's
    // terminator on some kernels).
    let key_eq = format!("{}=", key);
    while cur < buf.len() {
        let start = cur;
        while cur < buf.len() && buf[cur] != 0 {
            cur += 1;
        }
        if start == cur {
            break;
        }
        let entry = &buf[start..cur];
        if entry.starts_with(key_eq.as_bytes()) {
            let value = &entry[key_eq.len()..];
            return Some(String::from_utf8_lossy(value).into_owned());
        }
        if cur < buf.len() {
            cur += 1;
        }
    }
    None
}

/// BFS the process tree rooted at `root_pid` using a pre-fetched
/// `procs` table.  Returns every descendant in discovery order
/// (root NOT included).  Useful for "what's running under this PTY's
/// zsh".
pub fn descendants_of(root_pid: i32, procs: &[ProcRow]) -> Vec<ProcRow> {
    use std::collections::HashMap;
    // ppid → Vec<idx into procs>
    let mut by_ppid: HashMap<i32, Vec<usize>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        by_ppid.entry(p.ppid).or_default().push(i);
    }
    let mut out = Vec::new();
    let mut queue: Vec<i32> = Vec::new();
    queue.push(root_pid);
    while let Some(parent) = queue.pop() {
        if let Some(kids) = by_ppid.get(&parent) {
            for &idx in kids {
                let p = &procs[idx];
                out.push(p.clone());
                queue.push(p.pid);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_all_procs_returns_at_least_self() {
        let pid = std::process::id() as i32;
        let procs = list_all_procs();
        assert!(!procs.is_empty(), "sysctl returned empty proc table");
        assert!(
            procs.iter().any(|p| p.pid == pid),
            "self pid {} missing from sysctl table",
            pid
        );
    }

    #[test]
    fn proc_cwd_resolves_for_self() {
        let pid = std::process::id() as i32;
        let cwd = proc_cwd(pid).expect("proc_cwd None for self");
        assert!(cwd.is_absolute(), "cwd not absolute: {:?}", cwd);
    }

    #[test]
    fn proc_cmdline_contains_test_binary_path() {
        let pid = std::process::id() as i32;
        let line = proc_cmdline(pid).expect("proc_cmdline None for self");
        // Just check we got non-empty argv — the actual path will be
        // some cargo target/debug/deps/<hash> file.
        assert!(!line.is_empty(), "cmdline empty");
    }

    #[test]
    fn proc_env_value_reads_self_path() {
        // PATH is always set in a cargo-spawned test process.  We
        // don't pin to a specific value — just that we got SOMETHING
        // non-empty and matching what std::env reports.
        let pid = std::process::id() as i32;
        let path_via_proc = proc_env_value(pid, "PATH").expect("PATH None");
        let path_via_env = std::env::var("PATH").expect("env var PATH");
        assert_eq!(path_via_proc, path_via_env);
    }

    #[test]
    fn proc_env_value_returns_none_for_missing_key() {
        let pid = std::process::id() as i32;
        assert!(proc_env_value(pid, "MARSPOT_TEST_NOT_A_REAL_VAR_XYZ").is_none());
    }

    #[test]
    fn descendants_of_self_includes_at_most_self_subtree() {
        let pid = std::process::id() as i32;
        let procs = list_all_procs();
        let descendants = descendants_of(pid, &procs);
        // Self isn't included; descendants may or may not exist
        // depending on whether the test runner forked anything.
        assert!(!descendants.iter().any(|p| p.pid == pid));
    }
}

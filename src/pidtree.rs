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
    /// Wall-clock start time, unix seconds (`pbi_start_tvsec`).  Comes
    /// free with the `proc_bsdinfo` this table already fetches per pid.
    /// The claudecode plugin compares it against a session file's mtime
    /// to tell "this process is writing that session" from "that file
    /// belongs to something older than this process".
    pub start_unix: u64,
    /// `pbi_pgid` — this process's own process-group id.  A job's
    /// top-level process is the group leader, i.e. `pgid == pid`.
    pub pgid: i32,
    /// `e_tdev` — device number of the controlling terminal, 0 when the
    /// process has none.  Every process in one pane's PTY shares this,
    /// so it identifies "which pane is this".
    pub tty_dev: u32,
    /// `e_tpgid` — the **foreground** process group of this process's
    /// controlling terminal, i.e. what `tcgetpgrp()` on that tty would
    /// return.  The kernel reports the same value on every process
    /// sharing the tty, so one row answers "who currently owns the
    /// pane's keyboard" without opening the device.  0 / negative when
    /// there is no tty or no foreground group.
    pub tty_fg_pgid: i32,
}

/// Snapshot of every running process on the host (kernel-level scan
/// via `sysctl(KERN_PROC, KERN_PROC_ALL)`).  Equivalent to walking
/// `ps -A` without spawning a process.  Typically 300–600 rows on
/// a dev machine; the table is materialised into a Vec we own.
pub fn list_all_procs() -> Vec<ProcRow> {
    unsafe { list_all_procs_inner().unwrap_or_default() }
}

unsafe fn list_all_procs_inner() -> Option<Vec<ProcRow>> { unsafe {
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
        // None = exited between listpids and pidinfo.
        if let Some(row) = proc_row(pid) {
            out.push(row);
        }
    }
    Some(out)
}}

/// One process's row, without the table walk — a single
/// `proc_pidinfo(PROC_PIDTBSDINFO)`.  None when the pid is gone or the
/// call is refused.
pub fn proc_row(pid: i32) -> Option<ProcRow> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let r = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        );
        if r <= 0 {
            return None;
        }
        let comm = CStr::from_ptr(info.pbi_comm.as_ptr())
            .to_string_lossy()
            .into_owned();
        Some(ProcRow {
            pid,
            ppid: info.pbi_ppid as i32,
            comm,
            start_unix: info.pbi_start_tvsec,
            pgid: info.pbi_pgid as i32,
            tty_dev: info.e_tdev,
            tty_fg_pgid: info.e_tpgid as i32,
        })
    }
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

/// F3+4 — instantaneous resource snapshot for one pid.  Two
/// monotonically-growing cumulative counters plus RSS so the caller
/// can subtract two samples taken `dt` apart to derive CPU%.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcStat {
    /// Resident set size in bytes (RSS).
    pub rss_bytes: u64,
    /// Total CPU time consumed since the process started, in
    /// nanoseconds.  Sum of user + system time across all threads.
    /// Two samples taken `dt_ns` apart give CPU% via
    /// `(cur - prev) / dt_ns * 100.0` (per core).
    pub total_cpu_ns: u64,
    /// Number of threads at sample time (a "this pid spawned 30
    /// rustc workers" hint without walking the descendants list).
    pub threads: u32,
}

/// Sample one pid's stats via `proc_pidinfo(PROC_PIDTASKINFO)`.
/// Returns None when the process has exited / SIP blocks the call.
/// Cheap: one syscall, no allocation.
pub fn proc_stat(pid: i32) -> Option<ProcStat> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_taskinfo>() as i32,
        )
    };
    if r <= 0 {
        return None;
    }
    Some(ProcStat {
        rss_bytes: info.pti_resident_size,
        // `pti_total_user` / `pti_total_system` are absolute
        // nanoseconds since the process forked (libc::proc_taskinfo
        // documents the unit as ns — confirmed against `top -l 1`
        // matched within 0.1 %).
        total_cpu_ns: info.pti_total_user
            .saturating_add(info.pti_total_system),
        threads: info.pti_threadnum as u32,
    })
}

/// Full command line of `pid` via `sysctl(KERN_PROCARGS2)`.  Returns
/// the argv joined by spaces, or None on failure.  Empty argv → empty
/// string.
pub fn proc_cmdline(pid: i32) -> Option<String> {
    unsafe { proc_cmdline_inner(pid) }
}

unsafe fn proc_cmdline_inner(pid: i32) -> Option<String> { unsafe {
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
}}

/// Look up one environment variable on a running pid by walking
/// KERN_PROCARGS2 past argv into envp.  Returns the value of the
/// first match, or None when the var isn't set / the call is denied
/// (sandbox / SIP).
pub fn proc_env_value(pid: i32, key: &str) -> Option<String> {
    unsafe { proc_env_value_inner(pid, key) }
}

unsafe fn proc_env_value_inner(pid: i32, key: &str) -> Option<String> { unsafe {
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
}}

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

/// What currently owns a pane's keyboard, derived from the kernel's
/// own view of the PTY — no shell cooperation, no extra syscall (the
/// fields come off the `proc_bsdinfo` `list_all_procs` already reads).
///
/// This is the GENERIC layer of pane status: it knows about jobs and
/// terminals, not about what any particular job means.  A plugin that
/// understands the foreground program (e.g. claudecode reading its
/// jsonl) refines `Job` into program-specific states on top; nothing
/// here may grow that knowledge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneForeground {
    /// The shell's own process group owns the tty — zsh is sitting at
    /// its prompt with nothing running.
    AtPrompt,
    /// A job holds the tty.  `pgid` alone is the load-bearing part —
    /// "something the user started is running" — and it is always
    /// known.  `leader` names that job when it can be identified;
    /// `None` means the group owns the tty but no member of it was
    /// readable, which is a real state (leader exited a moment ago,
    /// or the one-pid probe path can't see the survivors).
    Job {
        pgid: i32,
        leader: Option<JobLeader>,
    },
    /// Can't tell: the shell pid is no longer in the table (pane
    /// exited), it has no controlling terminal, or the tty reports no
    /// foreground group — which is a real transient state between
    /// jobs and the steady state of an orphaned group.  Callers must
    /// treat this as "no information", never as "idle".
    Unknown,
}

/// The process at the head of a foreground job — what the user
/// launched (`claude`, `vim`, `cargo`, `ssh`), not whatever it has
/// since forked underneath.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLeader {
    pub pid: i32,
    /// `pbi_comm`, i.e. the first 16 bytes of the **executable file's**
    /// name.  A coarse label, NOT the program identity: probed against
    /// the 10 live claude panes on this host, every one reports
    /// `2.1.220` (claude execs a version-named file), not `claude`.  A
    /// caller that needs to know *which program* this is must read
    /// `proc_cmdline(pid)` — which is what the claudecode plugin's own
    /// `looks_like_claudecode` already does.
    pub comm: String,
    /// Start time, unix seconds.  Against the observation time this
    /// gives "how long has this job held the pane" with no history
    /// kept anywhere.
    pub start_unix: u64,
}

/// Foreground status of the pane whose PTY is headed by `shell_pid`
/// (the shelld session's `shell_child_pid` — the zsh at the top of the
/// pane's process tree), from a pre-fetched `procs` table.
///
/// Pure function of the table so it is testable without spawning
/// anything, and so one `list_all_procs()` serves every pane.  Callers
/// that don't already hold a table want `pane_foreground_probe`, which
/// costs 1–2 syscalls instead of one per pid on the host.
pub fn pane_foreground(shell_pid: i32, procs: &[ProcRow]) -> PaneForeground {
    let Some(shell) = procs.iter().find(|p| p.pid == shell_pid) else {
        return PaneForeground::Unknown;
    };
    let fg = match classify_shell(shell) {
        Ok(settled) => return settled,
        Err(fg) => fg,
    };
    // The foreground group's leader is the process whose pid equals
    // the pgid.  It can be gone while the group still owns the tty
    // (leader exited, children outlive it), so fall back to the
    // oldest surviving member of the group on the same tty — that is
    // the closest thing to "the job the user started".  Matching on
    // pgid alone would pick up another pane's job, since pgid numbers
    // are not per-terminal.
    let same_group = || {
        procs
            .iter()
            .filter(|p| p.pgid == fg && p.tty_dev == shell.tty_dev)
    };
    let leader = same_group()
        .find(|p| p.pid == fg)
        .or_else(|| same_group().min_by_key(|p| (p.start_unix, p.pid)));
    PaneForeground::Job {
        pgid: fg,
        leader: leader.map(|p| JobLeader {
            pid: p.pid,
            comm: p.comm.clone(),
            start_unix: p.start_unix,
        }),
    }
}

/// Same answer as `pane_foreground`, without walking the host's whole
/// process table: one `proc_pidinfo` on the shell, plus one on the
/// foreground group leader when a job is running.
///
/// This is the variant a per-second sweep over N panes should use —
/// `list_all_procs` costs a `proc_pidinfo` per pid on the box (~600 on
/// a dev machine), which is the wrong shape to pay every second for a
/// signal about 18 panes.  The cost is that the "leader exited but its
/// group still owns the tty" case yields `Job { leader: None }` rather
/// than the oldest surviving member: finding that member needs the
/// table.  Callers that already hold one (the claudecode scan does)
/// should use `pane_foreground` and get the better answer for free.
pub fn pane_foreground_probe(shell_pid: i32) -> PaneForeground {
    let Some(shell) = proc_row(shell_pid) else {
        return PaneForeground::Unknown;
    };
    let fg = match classify_shell(&shell) {
        Ok(settled) => return settled,
        Err(fg) => fg,
    };
    // Group leader = the pid equal to the pgid.  Verify it shares the
    // pane's tty before believing it: pids are recycled, and a stale
    // pgid pointing at an unrelated process on another terminal must
    // not be reported as this pane's job.
    let leader = proc_row(fg)
        .filter(|p| p.tty_dev == shell.tty_dev)
        .map(|p| JobLeader {
            pid: p.pid,
            comm: p.comm,
            start_unix: p.start_unix,
        });
    PaneForeground::Job { pgid: fg, leader }
}

/// Shared first half of both entry points: settle the cases that need
/// nothing but the shell's own row.  `Ok` = final answer, `Err(pgid)` =
/// a job owns the tty and its leader still has to be resolved.
fn classify_shell(shell: &ProcRow) -> Result<PaneForeground, i32> {
    // No controlling tty, or the tty has no foreground group.  Both
    // are "no information" — a pane whose shell lost its tty is not
    // an idle pane.
    if shell.tty_dev == 0 || shell.tty_fg_pgid <= 0 {
        return Ok(PaneForeground::Unknown);
    }
    if shell.tty_fg_pgid == shell.pgid {
        return Ok(PaneForeground::AtPrompt);
    }
    Err(shell.tty_fg_pgid)
}

/// F3+1 — one node of a process tree.  Same data as `ProcRow` plus
/// children list (depth-first nesting).  Used by the L2 process-tree
/// panel renderer to indent rows by depth.
#[derive(Debug, Clone)]
pub struct ProcNode {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub children: Vec<ProcNode>,
}

/// F3+1 — build a nested tree rooted at `root_pid` from a pre-fetched
/// proc table.  Returns None if `root_pid` itself isn't in the table
/// (process gone since the snapshot).  Children are ordered by pid
/// (deterministic across refresh ticks so the UI doesn't reshuffle
/// rows under the cursor on a frame where nothing changed).
pub fn tree_rooted_at(root_pid: i32, procs: &[ProcRow]) -> Option<ProcNode> {
    use std::collections::HashMap;
    let mut by_ppid: HashMap<i32, Vec<usize>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        by_ppid.entry(p.ppid).or_default().push(i);
    }
    let by_pid: HashMap<i32, &ProcRow> = procs.iter().map(|p| (p.pid, p)).collect();
    let root = by_pid.get(&root_pid)?;
    Some(build_node(root, procs, &by_ppid))
}

fn build_node(
    row: &ProcRow,
    procs: &[ProcRow],
    by_ppid: &std::collections::HashMap<i32, Vec<usize>>,
) -> ProcNode {
    let mut children: Vec<ProcNode> = by_ppid
        .get(&row.pid)
        .into_iter()
        .flat_map(|kids| kids.iter().copied())
        .map(|idx| build_node(&procs[idx], procs, by_ppid))
        .collect();
    children.sort_by_key(|n| n.pid);
    ProcNode {
        pid: row.pid,
        ppid: row.ppid,
        comm: row.comm.clone(),
        children,
    }
}

/// F3+1 — flatten a nested tree to a depth-tagged sequence for UI
/// row rendering.  `(depth, &node)` pairs in pre-order so root is
/// first, then children left-to-right.
pub fn flatten_pre_order<'a>(root: &'a ProcNode) -> Vec<(usize, &'a ProcNode)> {
    let mut out: Vec<(usize, &ProcNode)> = Vec::new();
    fn walk<'a>(node: &'a ProcNode, depth: usize, out: &mut Vec<(usize, &'a ProcNode)>) {
        out.push((depth, node));
        for c in &node.children {
            walk(c, depth + 1, out);
        }
    }
    walk(root, 0, &mut out);
    out
}

/// F3+1 — send `signal` to `pid`.  Thin wrapper over `libc::kill(2)`
/// so callers can `?` it without sprinkling `unsafe` everywhere.
/// Use `libc::SIGTERM` for graceful, `libc::SIGKILL` for force.
/// `pid > 0` only — passing 0 / -1 / a pgid is intentionally not
/// allowed here (the UI panel only ever has individual pids; the
/// "negative pgid = whole group" trick is a foot-gun in the
/// general case).
pub fn kill_pid(pid: i32, signal: i32) -> std::io::Result<()> {
    if pid <= 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to kill pid {pid} (must be > 1)"),
        ));
    }
    let r = unsafe { libc::kill(pid, signal) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// F3+1 — check whether `pid` is still alive.  Uses `kill(pid, 0)` —
/// the kernel performs the existence + permission check without
/// delivering a signal.  Returns false on ESRCH (gone) or EPERM
/// (alive but not ours).
pub fn pid_is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno == libc::EPERM
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

    #[test]
    fn tree_rooted_at_self_root_pid_matches() {
        let pid = std::process::id() as i32;
        let procs = list_all_procs();
        let node = tree_rooted_at(pid, &procs).expect("self not in proc table");
        assert_eq!(node.pid, pid);
        // Children deterministic order:
        let pids: Vec<i32> = node.children.iter().map(|c| c.pid).collect();
        let mut sorted = pids.clone();
        sorted.sort();
        assert_eq!(pids, sorted, "children not sorted by pid");
    }

    #[test]
    fn flatten_pre_order_visits_root_first_then_children_recursively() {
        // Construct a synthetic tree without OS calls so this test is
        // deterministic.
        let leaf_a = ProcNode { pid: 10, ppid: 5, comm: "a".into(), children: vec![] };
        let leaf_b = ProcNode { pid: 11, ppid: 5, comm: "b".into(), children: vec![] };
        let mid = ProcNode { pid: 5, ppid: 1, comm: "mid".into(), children: vec![leaf_a, leaf_b] };
        let root = ProcNode { pid: 1, ppid: 0, comm: "root".into(), children: vec![mid] };
        let flat: Vec<(usize, i32)> = flatten_pre_order(&root)
            .into_iter()
            .map(|(d, n)| (d, n.pid))
            .collect();
        assert_eq!(
            flat,
            vec![(0, 1), (1, 5), (2, 10), (2, 11)],
            "pre-order: root, mid, leaf_a, leaf_b at depths 0,1,2,2"
        );
    }

    #[test]
    fn pid_is_alive_self_true() {
        assert!(pid_is_alive(std::process::id() as i32));
    }

    #[test]
    fn pid_is_alive_invalid_false() {
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(1)); // init/launchd — never want to kill anyway
        assert!(!pid_is_alive(-1));
    }

    #[test]
    fn kill_pid_refuses_pid_le_1() {
        let err = kill_pid(0, libc::SIGTERM).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = kill_pid(1, libc::SIGTERM).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    // ── pane_foreground ───────────────────────────────────────────
    // Synthetic tables: the whole point of `pane_foreground` being a
    // pure function of the proc table is that these cases are
    // reachable without arranging real jobs on a real tty.

    /// Pane 1's tty.  Any non-zero dev_t; the value only has to be
    /// consistent within a table and different from other panes'.
    const TTY_A: u32 = 0x1000_0011;
    const TTY_B: u32 = 0x1000_0022;

    fn row(pid: i32, ppid: i32, comm: &str, pgid: i32, tty: u32, fg: i32, start: u64) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            comm: comm.into(),
            start_unix: start,
            pgid,
            tty_dev: tty,
            tty_fg_pgid: fg,
        }
    }

    /// zsh at its prompt: the tty's foreground group IS the shell's.
    #[test]
    fn pane_foreground_at_prompt_when_shell_owns_the_tty() {
        let procs = vec![row(500, 400, "zsh", 500, TTY_A, 500, 1000)];
        assert_eq!(pane_foreground(500, &procs), PaneForeground::AtPrompt);
    }

    /// A job in the foreground reports the job's leader, not the shell
    /// and not the leader's children.
    #[test]
    fn pane_foreground_reports_the_job_leader_not_its_children() {
        let procs = vec![
            row(500, 400, "zsh", 500, TTY_A, 600, 1000),
            row(600, 500, "claude", 600, TTY_A, 600, 2000),
            // A tool the job forked: same group, same tty, younger.
            row(601, 600, "cargo", 600, TTY_A, 600, 3000),
        ];
        assert_eq!(
            pane_foreground(500, &procs),
            PaneForeground::Job {
                pgid: 600,
                leader: Some(JobLeader {
                    pid: 600,
                    comm: "claude".into(),
                    start_unix: 2000,
                }),
            }
        );
    }

    /// Leader gone, group still owns the tty — report the oldest
    /// surviving member rather than losing the pane's status.
    #[test]
    fn pane_foreground_falls_back_to_oldest_member_when_leader_exited() {
        let procs = vec![
            row(500, 400, "zsh", 500, TTY_A, 600, 1000),
            row(602, 1, "node", 600, TTY_A, 600, 3000),
            row(601, 1, "node", 600, TTY_A, 600, 2500),
        ];
        assert_eq!(
            pane_foreground(500, &procs),
            PaneForeground::Job {
                pgid: 600,
                leader: Some(JobLeader {
                    pid: 601,
                    comm: "node".into(),
                    start_unix: 2500,
                }),
            }
        );
    }

    /// A job whose group has no readable member is still a job — the
    /// pane is NOT idle just because we failed to name what's running.
    /// (This is also the shape the one-pid probe returns for the
    /// leader-exited case, since naming the survivor needs the table.)
    #[test]
    fn pane_foreground_job_without_identifiable_leader_is_still_a_job() {
        let procs = vec![row(500, 400, "zsh", 500, TTY_A, 600, 1000)];
        assert_eq!(
            pane_foreground(500, &procs),
            PaneForeground::Job { pgid: 600, leader: None }
        );
    }

    /// Another pane's job must never be picked up: same pgid number
    /// can exist on a different tty, and pgid alone would match it.
    #[test]
    fn pane_foreground_ignores_same_pgid_on_a_different_tty() {
        let procs = vec![
            row(500, 400, "zsh", 500, TTY_A, 600, 1000),
            row(600, 500, "vim", 600, TTY_A, 600, 2000),
            row(700, 450, "zsh", 700, TTY_B, 600, 1000),
            row(701, 700, "ssh", 600, TTY_B, 600, 1500),
        ];
        match pane_foreground(500, &procs) {
            PaneForeground::Job { leader: Some(l), .. } => {
                assert_eq!((l.pid, l.comm.as_str()), (600, "vim"));
            }
            other => panic!("expected pane A's own job, got {other:?}"),
        }
    }

    /// "No information" cases stay Unknown — a caller deciding whether
    /// a pane is safe to touch must not read any of these as idle.
    #[test]
    fn pane_foreground_unknown_covers_every_no_information_case() {
        // Shell not in the table at all (pane exited).
        assert_eq!(pane_foreground(500, &[]), PaneForeground::Unknown);
        // No controlling tty.
        let no_tty = vec![row(500, 400, "zsh", 500, 0, 0, 1000)];
        assert_eq!(pane_foreground(500, &no_tty), PaneForeground::Unknown);
        // tty present, no foreground group (tcgetpgrp would say -1).
        let no_fg = vec![row(500, 400, "zsh", 500, TTY_A, -1, 1000)];
        assert_eq!(pane_foreground(500, &no_fg), PaneForeground::Unknown);
    }

    /// The probe path answers off the real kernel state for the test
    /// process itself.  `cargo`/`nextest` run it without a controlling
    /// terminal, so the honest answer is `Unknown`; under a tty (run
    /// the binary by hand) the same call reports the job.  Either way
    /// it must not panic and must not claim `AtPrompt` — asserting the
    /// tty-shape rather than a fixed variant keeps this from being a
    /// test of how the CI runner happens to attach stdio.
    #[test]
    fn pane_foreground_probe_agrees_with_this_process_own_tty_state() {
        let me = std::process::id() as i32;
        let row = proc_row(me).expect("proc_row None for self");
        let probed = pane_foreground_probe(me);
        if row.tty_dev == 0 || row.tty_fg_pgid <= 0 {
            assert_eq!(probed, PaneForeground::Unknown);
        } else if row.tty_fg_pgid == row.pgid {
            assert_eq!(probed, PaneForeground::AtPrompt);
        } else {
            assert!(
                matches!(probed, PaneForeground::Job { .. }),
                "tty fg group {} != own group {} should read as Job, got {probed:?}",
                row.tty_fg_pgid,
                row.pgid,
            );
        }
    }

    /// A pid that cannot exist has no row and no status.
    #[test]
    fn pane_foreground_probe_unknown_for_dead_pid() {
        assert!(proc_row(-1).is_none());
        assert_eq!(pane_foreground_probe(-1), PaneForeground::Unknown);
    }
}

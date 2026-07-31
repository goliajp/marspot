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
    /// `pbi_status` — `SRUN` / `SSLEEP` / `SSTOP` / `SZOMB`.  Comes off
    /// the same struct as everything above.  `SSTOP` is what tells a
    /// `^Z`-suspended job apart from one that simply is not in the
    /// foreground, and that distinction is the difference between "this
    /// pane is empty" and "this pane has the user's work parked in it".
    pub status: u32,
}

impl ProcRow {
    /// Suspended (`^Z`, `SIGSTOP`).  Not "not running" — a stopped
    /// process holds all of its state and resumes on `fg`.
    pub fn is_stopped(&self) -> bool {
        self.status == libc::SSTOP
    }

    /// Exited but not yet reaped.  Counts as neither running nor
    /// stopped; a zombie in a pane is not work in progress.
    pub fn is_zombie(&self) -> bool {
        self.status == libc::SZOMB
    }
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
            status: info.pbi_status,
        })
    }
}

/// Total CPU time this process has consumed, in nanoseconds (user +
/// system), via `proc_pidinfo(PROC_PIDTASKINFO)`.  None when the pid
/// is gone or the call is refused.
///
/// Sampled twice, the difference answers "is anything actually working
/// in here" — which is a different question from "does this process
/// have children", and the only one of the two that can be answered
/// honestly.  Measured on this host: every live claude keeps children
/// permanently (an MCP server, a language server, `caffeinate`, a
/// long-lived shell), so a child count says nothing about whether work
/// is in flight.
pub fn proc_cpu_time_ns(pid: i32) -> Option<u64> {
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let r = libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_taskinfo>() as i32,
        );
        if r <= 0 {
            return None;
        }
        Some(info.pti_total_user + info.pti_total_system)
    }
}

/// CPU time of `root` plus every descendant, nanoseconds.  Processes
/// that vanish mid-walk are skipped rather than failing the sum: this
/// feeds a "has anything moved since last time" comparison, and a
/// disappearing child is itself not work in flight.
pub fn subtree_cpu_time_ns(root: i32, procs: &[ProcRow]) -> u64 {
    let mut total = proc_cpu_time_ns(root).unwrap_or(0);
    for d in descendants_of(root, procs) {
        total = total.saturating_add(proc_cpu_time_ns(d.pid).unwrap_or(0));
    }
    total
}

/// Direct children of `pid`, via `proc_listchildpids` — one syscall,
/// no table walk.  Empty on error or when there are none.
///
/// The shell's direct children ARE its jobs: job control puts each one
/// in its own process group under the shell, whether it ends up in the
/// foreground, the background, or suspended.
pub fn child_pids(pid: i32) -> Vec<i32> {
    // Two things about this call are not what `proc_listpids` teaches,
    // and both were measured rather than assumed after the first
    // version of this function silently returned nothing:
    //
    //   1. The return value is the number of **entries** written, not
    //      a byte count.  One child returns 1.  Dividing it by
    //      `size_of::<pid_t>()` — the pattern the sibling API needs —
    //      yields 0, i.e. "this process has no children", for every
    //      process with fewer than four of them.
    //   2. The NULL-buffer sizing call does not size *these* results:
    //      asked about a process with exactly one child it answered
    //      971.  So there is nothing to size against; pass a buffer
    //      and grow it while it comes back full.
    const START: usize = 64;
    const MAX: usize = 4096;
    let mut cap = START;
    loop {
        let mut buf: Vec<libc::pid_t> = vec![0; cap];
        let n = unsafe {
            libc::proc_listchildpids(
                pid,
                buf.as_mut_ptr() as *mut libc::c_void,
                (buf.len() * std::mem::size_of::<libc::pid_t>()) as i32,
            )
        };
        if n <= 0 {
            return Vec::new();
        }
        let n = n as usize;
        if n < cap || cap >= MAX {
            buf.truncate(n.min(cap));
            return buf.into_iter().filter(|p| *p > 0).collect();
        }
        cap *= 4;
    }
}

/// The kernel half of a pane's state: who owns the tty, and what else
/// the shell is holding.
///
/// Cost is 2 syscalls for a quiet pane (the shell's row + its child
/// list) plus one per job, which at the shell's 1 s sweep is the same
/// order as the cwd sweep that already runs.
///
/// This replaces `pane_foreground`, which could not tell an empty pane
/// from one with a suspended job in it — both looked like "at a
/// prompt".
pub fn observe_pane(shell_pid: i32) -> crate::pane_state::Generic {
    let Some(shell) = proc_row(shell_pid) else {
        return crate::pane_state::Generic::Unknown;
    };
    // Only the foreground branch needs the leader, and only the
    // shell-in-front branch needs the children — fetch each lazily so
    // a busy pane costs 2 syscalls and a quiet one costs 2 + jobs.
    let fg_leader = (shell.tty_fg_pgid > 0 && shell.tty_fg_pgid != shell.pgid)
        .then(|| proc_row(shell.tty_fg_pgid))
        .flatten();
    let children: Vec<ProcRow> = if shell.tty_fg_pgid == shell.pgid {
        child_pids(shell_pid).into_iter().filter_map(proc_row).collect()
    } else {
        Vec::new()
    };
    classify_pane(&shell, fg_leader.as_ref(), &children)
}

/// The classification rules, as a pure function of rows already read —
/// so every branch is testable without arranging real jobs on a real
/// tty (which is exactly what "a pane with a suspended job" is
/// awkward to arrange in a unit test).
pub fn classify_pane(
    shell: &ProcRow,
    fg_leader: Option<&ProcRow>,
    children: &[ProcRow],
) -> crate::pane_state::Generic {
    use crate::pane_state::Generic;
    if shell.tty_dev == 0 || shell.tty_fg_pgid <= 0 {
        return Generic::Unknown;
    }
    if shell.tty_fg_pgid != shell.pgid {
        // A job owns the tty.  Believe a leader only if it shares the
        // pane's terminal: pids are recycled, and a stale pgid could
        // name an unrelated process on another tty.
        let leader = fg_leader
            .filter(|p| p.tty_dev == shell.tty_dev && p.pid == shell.tty_fg_pgid)
            .map(|p| JobLeader {
                pid: p.pid,
                comm: p.comm.clone(),
                start_unix: p.start_unix,
            });
        return Generic::Foreground { pgid: shell.tty_fg_pgid, leader };
    }
    // The shell owns the tty.  Whether the pane is actually empty
    // depends on what it is still holding: `^Z`-suspended jobs and
    // `cmd &` background jobs both live on with the shell in front,
    // and the old model could see neither.
    let (mut stopped, mut running) = (0u16, 0u16);
    for row in children {
        if row.is_zombie() {
            continue; // exited, just not reaped — not work in progress
        }
        if row.pgid == shell.pgid {
            continue; // in the shell's own group, so not a job
        }
        if row.is_stopped() {
            stopped = stopped.saturating_add(1);
        } else {
            running = running.saturating_add(1);
        }
    }
    if stopped == 0 && running == 0 {
        Generic::Idle
    } else {
        Generic::PromptWithJobs { stopped, running }
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
    use crate::pane_state::Generic;

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

    // ── pane classification ───────────────────────────────────────
    // Synthetic rows: the point of `classify_pane` being pure is that
    // "a pane with a suspended job in it" is a case you can assert on
    // without arranging one on a real tty.

    /// Pane 1's tty.  Any non-zero dev_t; the value only has to be
    /// consistent within a case and different from other panes'.
    const TTY_A: u32 = 0x1000_0011;
    const TTY_B: u32 = 0x1000_0022;

    #[allow(clippy::too_many_arguments)]
    fn row(
        pid: i32, ppid: i32, comm: &str, pgid: i32, tty: u32, fg: i32, start: u64,
        status: u32,
    ) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            comm: comm.into(),
            start_unix: start,
            pgid,
            tty_dev: tty,
            tty_fg_pgid: fg,
            status,
        }
    }

    fn shell(fg: i32) -> ProcRow {
        row(500, 400, "zsh", 500, TTY_A, fg, 1000, libc::SSLEEP)
    }

    /// Nothing running and nothing held: the only shape that may be
    /// called empty.
    #[test]
    fn classify_idle_needs_the_shell_in_front_and_no_jobs() {
        assert_eq!(classify_pane(&shell(500), None, &[]), Generic::Idle);
    }

    /// The regression the whole state machine exists for: a `^Z`'d job
    /// leaves the shell in the foreground, so the old model reported
    /// this pane as "at a prompt" — indistinguishable from empty.
    #[test]
    fn classify_a_suspended_job_is_not_idle() {
        let kids = [row(600, 500, "claude", 600, TTY_A, 500, 2000, libc::SSTOP)];
        assert_eq!(
            classify_pane(&shell(500), None, &kids),
            Generic::PromptWithJobs { stopped: 1, running: 0 }
        );
    }

    #[test]
    fn classify_a_background_job_is_not_idle_either() {
        let kids = [row(600, 500, "cargo", 600, TTY_A, 500, 2000, libc::SRUN)];
        assert_eq!(
            classify_pane(&shell(500), None, &kids),
            Generic::PromptWithJobs { stopped: 0, running: 1 }
        );
    }

    /// A zombie is an exited job, not work in progress; a child that
    /// shares the shell's own process group is not a job at all.
    #[test]
    fn classify_ignores_zombies_and_the_shells_own_group() {
        let kids = [
            row(600, 500, "gone", 600, TTY_A, 500, 2000, libc::SZOMB),
            row(601, 500, "helper", 500, TTY_A, 500, 2000, libc::SRUN),
        ];
        assert_eq!(classify_pane(&shell(500), None, &kids), Generic::Idle);
    }

    /// A job in the foreground names its leader.
    #[test]
    fn classify_foreground_reports_the_leader() {
        let leader = row(600, 500, "claude", 600, TTY_A, 600, 2000, libc::SRUN);
        assert_eq!(
            classify_pane(&shell(600), Some(&leader), &[]),
            Generic::Foreground {
                pgid: 600,
                leader: Some(JobLeader {
                    pid: 600,
                    comm: "claude".into(),
                    start_unix: 2000,
                }),
            }
        );
    }

    /// A leader row from a DIFFERENT tty must not be believed — pids
    /// are recycled, and pgid numbers are not per-terminal.
    #[test]
    fn classify_rejects_a_leader_row_from_another_tty() {
        let impostor = row(600, 1, "vim", 600, TTY_B, 600, 2000, libc::SRUN);
        assert_eq!(
            classify_pane(&shell(600), Some(&impostor), &[]),
            Generic::Foreground { pgid: 600, leader: None },
            "a job we cannot name is still a job"
        );
    }

    /// "No information" cases stay Unknown — a caller deciding whether
    /// a pane is safe to touch must not read any of these as idle.
    #[test]
    fn classify_unknown_covers_the_no_information_cases() {
        let no_tty = row(500, 400, "zsh", 500, 0, 0, 1000, libc::SSLEEP);
        assert_eq!(classify_pane(&no_tty, None, &[]), Generic::Unknown);
        let no_fg = row(500, 400, "zsh", 500, TTY_A, -1, 1000, libc::SSLEEP);
        assert_eq!(classify_pane(&no_fg, None, &[]), Generic::Unknown);
    }

    /// The syscall wrapper agrees with the kernel about this very
    /// process.  Under `cargo`/`nextest` there is no controlling
    /// terminal, so the honest answer is Unknown; asserting the shape
    /// rather than a fixed variant keeps this from testing how the CI
    /// runner attaches stdio.
    #[test]
    fn observe_pane_agrees_with_this_process_own_tty_state() {
        let me = std::process::id() as i32;
        let r = proc_row(me).expect("proc_row None for self");
        let observed = observe_pane(me);
        if r.tty_dev == 0 || r.tty_fg_pgid <= 0 {
            assert_eq!(observed, Generic::Unknown);
        } else if r.tty_fg_pgid == r.pgid {
            assert!(matches!(
                observed,
                Generic::Idle | Generic::PromptWithJobs { .. }
            ));
        } else {
            assert!(matches!(observed, Generic::Foreground { .. }));
        }
    }

    /// End-to-end on a real PTY with a real shell: run a job, `^Z` it,
    /// and require the observation to change from `Foreground` to
    /// `PromptWithJobs { stopped: 1 }`.
    ///
    /// This is the case the whole state machine was built for, and it
    /// is exactly the one the synthetic rows above cannot prove: that
    /// the kernel really does report a suspended job the way
    /// `classify_pane` assumes (child of the shell, own process group,
    /// `SSTOP`, with the shell back in front of the tty).  `^Z` is sent
    /// as the byte the line discipline turns into SIGTSTP, not as a
    /// signal we deliver ourselves, so the path is the user's path.
    #[test]
    fn observe_pane_sees_a_suspended_job_on_a_real_pty() {
        use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
        use std::time::{Duration, Instant};

        let mut pty = Pty::spawn(PtyConfig {
            // `-f` skips rc files: this test is about the kernel's
            // view, and the user's zsh setup is not part of it.
            program: "/bin/zsh".into(),
            args: vec!["-f".into()],
            size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
            argv0: None,
            cwd: Some("/".into()),
            env_remove_prefixes: vec!["MARSPOT_".into()],
        })
        .expect("spawn zsh on a pty");
        let shell_pid = pty.child_pid();

        // Drain whatever the shell prints; a full pty buffer would
        // block it and stall the test for reasons unrelated to what is
        // under test.  The master fd is blocking by default, so it has
        // to be switched first — a blocking read on a quiet shell is a
        // hang, not a drain (learned the direct way).
        unsafe {
            let fd = pty.raw_master();
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let drain = |p: &Pty| {
            let mut buf = [0u8; 4096];
            while p.read_shared(&mut buf).unwrap_or(0) > 0 {}
        };
        let poll = |p: &Pty, want: &str, f: &dyn Fn(&Generic) -> bool| -> Generic {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                drain(p);
                let g = observe_pane(shell_pid);
                if f(&g) {
                    return g;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {want}; last observation {g:?}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        };

        // Wait for the shell itself to settle at a prompt first, so a
        // slow startup can't be mistaken for the job we start next.
        poll(&pty, "the shell to reach its prompt", &|g| {
            matches!(g, Generic::Idle)
        });

        pty.write(b"sleep 30\n").expect("write to the pty");
        let fg = poll(&pty, "the job to take the tty", &|g| {
            matches!(g, Generic::Foreground { .. })
        });
        let Generic::Foreground { leader, .. } = &fg else {
            unreachable!()
        };
        assert_eq!(
            leader.as_ref().map(|l| l.comm.as_str()),
            Some("sleep"),
            "the foreground job should be the one we started"
        );

        // 0x1a = ^Z.  The line discipline turns it into SIGTSTP for
        // the foreground group.
        pty.write(&[0x1a]).expect("send ^Z");
        let suspended = poll(&pty, "the job to be suspended", &|g| {
            matches!(g, Generic::PromptWithJobs { .. })
        });
        assert_eq!(
            suspended,
            Generic::PromptWithJobs { stopped: 1, running: 0 },
            "a ^Z'd job must read as one stopped job, not as an idle pane"
        );
        // …and the composed state must not be quiet.
        let status = crate::pane_state::compose(
            &suspended,
            crate::pane_state::Activity::AwaitingUser,
            true,
        );
        assert!(
            !status.is_quiet(),
            "composed state {status:?} must not be quiet with a job parked in the pane"
        );

        // Tear down what this test parked in the pane, BEFORE `Pty`'s
        // Drop runs: a stopped job left behind makes that Drop block
        // (measured — the test sat in it for 13 minutes until nextest
        // SIGKILLed the process).  Whether Drop should survive a
        // stopped job on its own is a separate question about `Pty`;
        // this test is about the classification and cleans up after
        // itself either way.
        for pid in child_pids(shell_pid) {
            if let Some(row) = proc_row(pid) {
                unsafe {
                    libc::killpg(row.pgid, libc::SIGCONT);
                    libc::killpg(row.pgid, libc::SIGKILL);
                }
            }
        }
    }

    /// Pins the calling convention against a real child.  The first
    /// version of `child_pids` treated the return value as a byte
    /// count and so reported "no children" for every process with
    /// fewer than four — which made the whole suspended-job detection
    /// inert while every synthetic test still passed.
    #[test]
    fn child_pids_finds_a_real_child_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .expect("spawn /bin/sleep");
        // The child is visible as soon as fork returns; no wait needed
        // beyond the spawn itself.
        let me = std::process::id() as i32;
        let kids = child_pids(me);
        let found = kids.contains(&(child.id() as i32));
        let _ = child.kill();
        let _ = child.wait();
        assert!(found, "spawned child {} missing from {:?}", child.id(), kids);
    }

    /// CPU time is monotonic and moves when work happens — the two
    /// properties the idle policy relies on.
    #[test]
    fn proc_cpu_time_ns_is_monotonic_and_grows_under_load() {
        let me = std::process::id() as i32;
        let before = proc_cpu_time_ns(me).expect("cpu time for self");
        // Burn a measurable slice of CPU without sleeping (a sleep
        // would prove nothing — the point is that *work* moves it).
        let mut acc = 0u64;
        for i in 0..8_000_000u64 {
            acc = acc.wrapping_add(i ^ acc.rotate_left(7));
        }
        std::hint::black_box(acc);
        let after = proc_cpu_time_ns(me).expect("cpu time for self");
        assert!(after >= before, "cpu time went backwards: {before} → {after}");
        assert!(
            after > before,
            "burning 8M iterations must show up as CPU time ({before} → {after})"
        );
        assert!(proc_cpu_time_ns(-1).is_none());
    }

    #[test]
    fn child_pids_of_a_pid_with_no_children_is_empty() {
        // pid 1 (launchd) has children; a pid that cannot exist has
        // none and must not panic.
        assert!(child_pids(-1).is_empty());
    }

    /// A pid that cannot exist has no row and no state.
    #[test]
    fn observe_pane_unknown_for_dead_pid() {
        assert!(proc_row(-1).is_none());
        assert_eq!(observe_pane(-1), Generic::Unknown);
    }
}

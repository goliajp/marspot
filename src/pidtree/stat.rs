//! Resource samples of one pid: RSS, cumulative CPU time, threads.

/// F3+4 — instantaneous resource snapshot for one pid.  Two
/// monotonically-growing cumulative counters plus RSS so the caller
/// can subtract two samples taken `dt` apart to derive CPU%.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcStat {
    /// Resident set size in bytes (RSS).
    pub rss_bytes: u64,
    /// Total CPU time consumed since the process started, in
    /// nanoseconds, user + system across all threads.  The kernel
    /// reports mach ticks (125/3 ns each on Apple silicon); this is
    /// already converted.  Two samples give CPU% via `cpu_pct_since`.
    pub total_cpu_ns: u64,
    /// Number of threads at sample time (a "this pid spawned 30
    /// rustc workers" hint without walking the descendants list).
    pub threads: u32,
}

impl ProcStat {
    /// CPU% (100 = one core) since an earlier sample that read
    /// `prev_cpu_ns` and was taken `dt_ns` ago; 0 when either is unusable
    pub fn cpu_pct_since(&self, prev_cpu_ns: u64, dt_ns: u64) -> f32 {
        if dt_ns == 0 {
            return 0.0;
        }
        (self.total_cpu_ns.saturating_sub(prev_cpu_ns) as f64 / dt_ns as f64 * 100.0) as f32
    }
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
        total_cpu_ns: super::probes::ticks_to_ns(info.pti_total_user.saturating_add(info.pti_total_system)),
        threads: info.pti_threadnum as u32,
    })
}

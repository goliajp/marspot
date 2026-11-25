//! Per-process reads the table does not carry: CPU time, an open
//! file's path, and the processes that exited but were never reaped.

use std::ffi::CStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::OnceLock;

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
        Some(ticks_to_ns(info.pti_total_user + info.pti_total_system))
    }
}

/// CPU time of `pid` itself plus every child it has reaped, in
/// nanoseconds, via `proc_pid_rusage(RUSAGE_INFO_V2)`.  None when the
/// pid is gone or the call is refused.
///
/// A script that sleeps and starts a short command each minute spends
/// nearly all of its CPU in children that are gone by the next look;
/// its own time stays near zero.  Children reaped by someone else (an
/// orphan's by launchd) are not counted here.
pub fn cpu_with_children(pid: i32) -> Option<u64> {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a rusage_info_v2, the flavor asked for
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut info).cast()) };
    if rc != 0 {
        return None;
    }
    let ticks = info.ri_user_time + info.ri_system_time + info.ri_child_user_time + info.ri_child_system_time;
    Some(ticks_to_ns(ticks))
}

/// The task and rusage times are mach ticks, not nanoseconds: on Apple silicon
/// one tick is 125/3 ns.
fn ticks_to_ns(ticks: u64) -> u64 {
    #[repr(C)]
    struct MachTimebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
    }
    static RATIO: OnceLock<(u32, u32)> = OnceLock::new();
    let (numer, denom) = *RATIO.get_or_init(|| {
        let mut tb = MachTimebase { numer: 0, denom: 0 };
        // SAFETY: `tb` matches the C struct the kernel fills
        unsafe { mach_timebase_info(&mut tb) };
        (tb.numer, tb.denom)
    });
    (ticks as u128 * numer as u128 / denom as u128) as u64
}

/// `struct proc_fileinfo` from `<sys/proc_info.h>`.
#[repr(C)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

/// `struct vnode_fdinfowithpath`.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// The path of the file `pid` has open as `fd`, via
/// `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`.  None when the process or
/// the descriptor is gone, the descriptor is not a vnode (a pipe, a
/// socket, a pty reads as its device path), or the call is refused.
pub fn fd_path(pid: i32, fd: i32) -> Option<PathBuf> {
    let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int;
    // SAFETY: `info` is a vnode_fdinfowithpath of `size` bytes
    let n = unsafe { libc::proc_pidfdinfo(pid, fd, PROC_PIDFDVNODEPATHINFO, (&raw mut info).cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: the kernel NUL-terminates vip_path within its MAXPATHLEN bytes
    let path = unsafe { CStr::from_ptr(info.pvip.vip_path.as_ptr().cast::<libc::c_char>()) };
    let bytes = path.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

/// A process that has exited and is waiting for its parent to reap it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zombie {
    pub pid: i32,
    pub ppid: i32,
    pub start_us: u64,
}

// `struct kinfo_proc` (libc does not declare it): 648 bytes on 64-bit,
// `kp_proc` (extern_proc) first, `kp_eproc` after it.  Offsets checked
// against `ps` on arm64.
const KINFO_SIZE: usize = 648;
const P_STARTTIME: usize = 0;
const P_STAT: usize = 36;
const P_PID: usize = 40;
const E_PPID: usize = 560;
const SZOMB: u8 = 5;

/// Every zombie on the host, from one `sysctl(KERN_PROC_ALL)`.
///
/// The table this module builds cannot show them: `proc_pidinfo`
/// answers nothing for a zombie, so it never gets a row.  Empty when
/// the sysctl fails.
pub fn zombies() -> Vec<Zombie> {
    let buf = kinfo_all();
    buf.chunks_exact(KINFO_SIZE)
        .filter(|kp| kp[P_STAT] == SZOMB)
        .map(|kp| Zombie {
            pid: i32_at(kp, P_PID),
            ppid: i32_at(kp, E_PPID),
            start_us: (i64_at(kp, P_STARTTIME).max(0) as u64)
                .saturating_mul(1_000_000)
                .saturating_add(i32_at(kp, P_STARTTIME + 8).max(0) as u64),
        })
        .collect()
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    i64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

fn kinfo_all() -> Vec<u8> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL];
    // the table can grow between sizing and reading; a few tries with room to spare
    for _ in 0..4 {
        let mut len: libc::size_t = 0;
        // SAFETY: a null buffer asks only for the size
        let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; len + 64 * KINFO_SIZE];
        let mut len = buf.len();
        // SAFETY: `buf` holds `len` writable bytes
        let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        if rc == 0 {
            buf.truncate(len - len % KINFO_SIZE);
            return buf;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return Vec::new();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests;

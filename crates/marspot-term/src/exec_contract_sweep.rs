//! Before an exec: what it does not carry is taken out of its way.

use std::os::fd::RawFd;
use std::process::Command;

use super::{ExecKind, KIND_ENV};

pub(super) fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The `MARSPOT_*` variables of this process's environment.
pub(super) fn own_marspot_vars() -> Vec<String> {
    std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).filter(|k| k.starts_with("MARSPOT_")).collect()
}

/// Ready `cmd`'s environment for an exec of `kind`: every `MARSPOT_*`
/// variable it does not carry is removed, and the kind is named.  A
/// variable set on `cmd` that the kind does not list is a mistake at
/// the exec site: a panic in debug builds, removed in others.
pub fn sweep_env(kind: ExecKind, cmd: &mut Command) {
    let mut set = Vec::new();
    for (k, v) in cmd.get_envs() {
        let Some(k) = k.to_str() else { continue };
        if v.is_some() && !kind.carries(k) {
            if cfg!(debug_assertions) {
                panic!("{k} is set for a {} exec but not listed for it", kind.name());
            }
            crate::lx_warn!("exec.contract.unlisted", "a variable set for this exec is not listed for it", var = k, kind = kind.name());
        }
        set.push((k.to_string(), v.is_some() && kind.carries(k)));
    }
    for (k, keep) in &set {
        if !keep {
            cmd.env_remove(k);
        }
    }
    for k in own_marspot_vars() {
        if !kind.carries(&k) && !set.iter().any(|(s, _)| *s == k) {
            cmd.env_remove(&k);
        }
    }
    cmd.env(KIND_ENV, kind.mark());
}

/// Ready this process for replacing its own image with an exec of
/// `kind` through `cmd`: [`sweep_env`], and every descriptor the kind
/// does not carry is marked close-on-exec.
pub fn prepare_command_exec(kind: ExecKind, cmd: &mut Command, handed: &[RawFd]) {
    let set = |name: &str| {
        let name = std::ffi::OsStr::new(name);
        match cmd.get_envs().find(|(k, _)| *k == name) {
            Some((_, v)) => v.and_then(|v| v.to_str()).map(str::to_string),
            None => process_env(name.to_str()?),
        }
    };
    let keep = kind.carried(set, handed);
    sweep_env(kind, cmd);
    sweep_fds(&keep);
}

/// The same for an exec through `execv` with this process's own
/// environment.
///
/// # Safety
/// Changes the environment: no other thread may be reading it.
pub unsafe fn prepare_own_exec(kind: ExecKind, handed: &[RawFd]) {
    let keep = kind.carried(process_env, handed);
    for k in own_marspot_vars() {
        if !kind.carries(&k) {
            // SAFETY: the caller's
            unsafe { std::env::remove_var(&k) };
        }
    }
    // SAFETY: the caller's
    unsafe { std::env::set_var(KIND_ENV, kind.mark()) };
    sweep_fds(&keep);
}

/// The descriptors from 3 up that are open without `FD_CLOEXEC`.
pub fn open_without_cloexec() -> Vec<RawFd> {
    // the directory's own descriptor has the flag, so it is not listed
    let Ok(dir) = std::fs::read_dir("/dev/fd") else { return Vec::new() };
    let mut fds: Vec<RawFd> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd: &RawFd| {
            fd >= 3 && {
                // SAFETY: F_GETFD on a number; a closed one answers -1
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                flags >= 0 && flags & libc::FD_CLOEXEC == 0
            }
        })
        .collect();
    fds.sort_unstable();
    fds
}

/// Mark every descriptor from 3 up but those in `keep` close-on-exec.
pub fn sweep_fds(keep: &[RawFd]) {
    for fd in open_without_cloexec() {
        if !keep.contains(&fd) {
            // SAFETY: fcntl on a descriptor this process holds
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

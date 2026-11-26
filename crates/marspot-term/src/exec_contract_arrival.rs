//! The new image's side: what it was handed, against what its kind of
//! exec carries.

use std::os::fd::RawFd;

use super::{ExecKind, KIND_ENV, open_without_cloexec, own_marspot_vars, process_env};

/// What an image started by a marspot exec found at its start.
#[derive(Debug)]
pub struct Arrival {
    kind: ExecKind,
    fds: Vec<RawFd>,
    vars: Vec<String>,
    /// the [`super::Carried::NumberIn`] numbers, read before anything
    /// takes their variables out
    numbered: Vec<RawFd>,
}

/// First thing in `main`: what the exec that started this image handed
/// it, and the kind's mark out of the environment.  `None` when no
/// marspot exec started it, or one of a kind this build does not know,
/// or the mark was made by neither this process nor its parent.
///
/// # Safety
/// Changes the environment: no other thread may exist yet.
pub unsafe fn arrive() -> Option<Arrival> {
    let mark = std::env::var(KIND_ENV).ok()?;
    // SAFETY: the caller's
    unsafe { std::env::remove_var(KIND_ENV) };
    let (name, pid) = mark.split_once(':')?;
    let kind = ExecKind::parse(name)?;
    // SAFETY: neither call can fail
    let maker = if kind.replaces_itself() { unsafe { libc::getpid() } } else { unsafe { libc::getppid() } };
    if pid.parse::<libc::pid_t>().ok()? != maker {
        return None;
    }
    Some(Arrival {
        kind,
        fds: open_without_cloexec(),
        vars: own_marspot_vars(),
        numbered: kind.carried(process_env, &[]),
    })
}

impl Arrival {
    pub fn kind(&self) -> ExecKind {
        self.kind
    }

    /// The descriptors and variables that crossed without being carried;
    /// `handed` are the ones the previous image named in its handoff.
    pub fn mismatch(&self, handed: &[RawFd]) -> Option<String> {
        let fds: Vec<RawFd> =
            self.fds.iter().copied().filter(|fd| !self.numbered.contains(fd) && !handed.contains(fd)).collect();
        let vars: Vec<&str> = self.vars.iter().map(String::as_str).filter(|v| !self.kind.carries(v)).collect();
        if fds.is_empty() && vars.is_empty() {
            return None;
        }
        Some(format!("{} exec handed fds {fds:?} and variables {vars:?} it does not carry", self.kind.name()))
    }

    /// Log a mismatch; in debug builds, which every test runs, panic.
    pub fn verify(&self, handed: &[RawFd]) {
        let Some(m) = self.mismatch(handed) else { return };
        crate::lx_error!("exec.contract.mismatch", &m, kind = self.kind.name());
        if cfg!(debug_assertions) {
            panic!("{m}");
        }
    }
}

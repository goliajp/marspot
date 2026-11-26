//! The one way this binary replaces its own image, and what has to end
//! before it does.
//!
//! Whatever owns something an exec would leave behind (a relay waiting
//! on a pipe, a kill still queued) registers a hook here at start-up:
//! `before` runs right before the exec, `failed` when it returned.  The
//! table knows hooks by name only, so the exec sites name no owner.

use std::sync::Mutex;

/// Hooks a binary may register; a handful exist.
const CAP: usize = 8;

#[derive(Clone, Copy)]
pub(crate) struct ExecHook {
    pub(crate) name: &'static str,
    pub(crate) before: fn(),
    pub(crate) failed: Option<fn()>,
}

static HOOKS: Mutex<Vec<ExecHook>> = Mutex::new(Vec::new());

/// Add `hook`, or replace the one of the same name in its place: the
/// hooks run in the order they were first registered.
pub(crate) fn register(hook: ExecHook) {
    let mut hooks = HOOKS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(h) = hooks.iter_mut().find(|h| h.name == hook.name) {
        *h = hook;
        return;
    }
    assert!(hooks.len() < CAP, "exec hook table full at {}", hook.name);
    hooks.push(hook);
}

/// The registered hooks' names, in the order they run.
#[cfg(test)]
pub(crate) fn names() -> Vec<&'static str> {
    snapshot().iter().map(|h| h.name).collect()
}

fn snapshot() -> Vec<ExecHook> {
    HOOKS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Exec `cmd` as this process's next image, after every hook's
/// `before`.  Returns only when the exec failed, after every hook's
/// `failed`.
pub(crate) fn exec_prepared(cmd: &mut std::process::Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    // copied out: a hook may register, and the lock is not held across the exec
    let hooks = snapshot();
    for h in &hooks {
        (h.before)();
    }
    let err = cmd.exec();
    for f in hooks.iter().filter_map(|h| h.failed) {
        f();
    }
    err
}

#[cfg(test)]
#[path = "exec_hooks_tests.rs"]
mod tests;

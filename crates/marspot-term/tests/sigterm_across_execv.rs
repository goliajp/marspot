//! A SIGTERM that lands mid-`execv` must not kill the process.
//!
//! This is the failure that lost five panes on 2026-09-07: two
//! independent SIGTERM fan-outs reach the same L3 in one install, and
//! the second one arrived while the process was between `execv` and
//! its new image arming a handler.  With no handler installed, the
//! default action terminated it — no log line, no crash report, and
//! the install's own check reported success because it only counted
//! survivors.
//!
//! The test re-execs the test binary itself, which is what makes it a
//! real handoff rather than a simulation of one.
use std::process::Command;

const ROLE: &str = "MARSPOT_TEST_EXECV_ROLE";

/// Second half of the handoff: arm a handler, then unblock.  Prints a
/// line only if it is alive to do so.
fn child() -> ! {
    // The mask was inherited across execv; a SIGTERM raised before it
    // is pending right now.
    let blocked = marspot_term::signals::sigterm_blocked();
    let _ = &blocked;
    unsafe {
        // A handler that does nothing is enough: the point is that the
        // default action (terminate) is no longer what happens.
        extern "C" fn noop(_: libc::c_int) {}
        libc::signal(libc::SIGTERM, noop as libc::sighandler_t);
    }
    marspot_term::signals::set_sigterm_blocked(false);
    // An exit code, not a print: the test harness captures `println!`
    // from inside a test, so a message here would never reach this
    // process's stdout and the assertion would be checking nothing.
    std::process::exit(if blocked { 7 } else { 8 });
}

/// First half: raise SIGTERM at itself — standing in for the second
/// fan-out — and then execv.
fn parent(block: bool) -> ! {
    if block {
        marspot_term::signals::set_sigterm_blocked(true);
    }
    unsafe { libc::raise(libc::SIGTERM) };
    let exe = std::env::current_exe().expect("current exe");
    let c = std::ffi::CString::new(exe.as_os_str().to_str().unwrap()).unwrap();
    unsafe { std::env::set_var(ROLE, "child") };
    let argv = [c.as_ptr(), std::ptr::null()];
    unsafe { libc::execv(c.as_ptr(), argv.as_ptr()) };
    std::process::exit(2);
}

fn run(role: &str) -> std::process::Output {
    let exe = std::env::current_exe().expect("current exe");
    Command::new(exe).env(ROLE, role).output().expect("spawn")
}

#[test]
fn dispatch_roles() {
    // Cargo runs every #[test]; the re-exec'd copies land here with a
    // role set and must do their job instead of running the suite.
    match std::env::var(ROLE).as_deref() {
        Ok("child") => child(),
        Ok("parent-block") => parent(true),
        Ok("parent-plain") => parent(false),
        _ => {}
    }
}

#[test]
fn a_signal_arriving_mid_execv_is_delivered_not_fatal() {
    if std::env::var(ROLE).is_ok() {
        return;
    }
    // Unblocked: this is what killed the five panes.
    let plain = run("parent-plain");
    assert_eq!(
        plain.status.code(),
        None,
        "unblocked, the process is expected to die by signal, not exit"
    );
    assert_ne!(plain.status.code(), Some(7), "the new image never got to run");

    // Blocked across the handoff: the signal waits.
    let blocked = run("parent-block");
    assert_eq!(
        blocked.status.code(),
        Some(7),
        "the image must come up AND find SIGTERM still blocked — \
         7 = survived with the mask inherited, 8 = survived without it"
    );
}

//! The shell with no window: what stays of the app after the user quits
//! it while sessions run.
//!
//! Sessions outlive a quit, and they stay in the coalition of the shell
//! that started them.  LaunchServices holds this process answerable for
//! that coalition for as long as it is registered and alive; were it to
//! exit, the next program to register from a pane would have every
//! session listed as its own (`marspot_term::owner`).  So a quit with
//! sessions kept does not end the process.  It replaces its own image
//! with this: no window, no core, no plugin, no Dock icon, nothing
//! mapped that the app needed, asleep until one of three things
//! happens.  The user opens the app again, and this process becomes the
//! app again, same pid, same coalition.  The last session ends, and it
//! exits for real.  An update is staged, and it moves to the new image.

use std::os::unix::process::CommandExt;

use marspot::{lx_event, lx_warn};
use marspot_term::owner::{HEIR_ENV, RESIDENT_ENV};
use marspot_term::session_registry;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSApplicationTerminateReply,
};

use crate::supervisor::BinaryTree;
use crate::{launch, ownership, sup_log};

/// Set for the app that a resident becomes when an update was staged
/// while it had no core to hand it to.
const UPDATE_WAITING_ENV: &str = "MARSPOT_UPDATE_WAITING";

pub(crate) fn asked() -> bool {
    std::env::var_os(RESIDENT_ENV).is_some()
}

/// Whether this app was a resident that left staged binaries for it.
/// Consumes the mark.
pub(crate) fn update_was_waiting() -> bool {
    let waiting = std::env::var_os(UPDATE_WAITING_ENV).is_some();
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(UPDATE_WAITING_ENV) };
    waiting
}

/// The pids of the sessions the registry names that are running.
fn live_sessions() -> Vec<i32> {
    session_registry::list_session_entries()
        .into_iter()
        .map(|e| e.pid)
        .filter(|pid| session_registry::pid_is_live_session(*pid))
        .collect()
}

/// Replace this image with `exe`, same pid and coalition, as a resident
/// or as the app.  Returns only when the exec failed.
fn become_image(exe: &std::path::Path, resident: bool, extra: &[(&str, &str)]) -> std::io::Error {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut cmd = std::process::Command::new(exe);
    if let Some(arg0) = args.first() {
        cmd.arg0(arg0);
    }
    cmd.args(args.iter().skip(1)).env_remove(RESIDENT_ENV).env_remove(HEIR_ENV);
    if resident {
        cmd.env(RESIDENT_ENV, "1");
        // the dev seams were for the app that is being put away; the
        // mark of a sandbox is not one of them
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|n| n.starts_with("MARSPOT_DEV_") && n != "MARSPOT_DEV_SANDBOX") {
                cmd.env_remove(name);
            }
        }
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
    // no launch may take the lock between the two images: it would be
    // an app outside the sessions' coalition
    crate::instance::carry_lock(&mut cmd);
    let err = cmd.exec();
    crate::instance::keep_lock_here();
    err
}

fn own_image() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| ownership::successor_exe())
}

/// The argument of the instance `replace_dead_record` has started.
pub(crate) const LS_REFRESH_ARG: &str = "--ls-refresh";

/// Be an instance of the app for a moment and exit, touching nothing.
pub(crate) fn ls_refresh_main() -> ! {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        app.finishLaunching();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    std::process::exit(0)
}

/// For a shell a session started, after the shell died or for one
/// that came up outside the coalition: have LaunchServices start the
/// bundle once more, before this process registers.
///
/// The dead shell's record stays, listing the coalition's members,
/// until the app is started again; then the coalition is nobody's, and
/// the first program to register from inside it has the members listed
/// as its own.  Started from here that moment is ours: the instance
/// registers, replaces the dead record and exits, and this process
/// registers right after it.  Measured on macOS 27.2: without this the
/// members stay on the dead record, and the user opening the app is
/// what frees them; an instance that exits without registering changes
/// nothing.
pub(crate) fn replace_dead_record() {
    let exe = ownership::successor_exe();
    let Some(bundle) = ownership::bundle_of(&exe) else { return };
    let begun = std::time::Instant::now();
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-n", "-g", "-j", "-W"])
        .arg(bundle)
        .args(["--args", LS_REFRESH_ARG])
        .stdin(std::process::Stdio::null())
        .status();
    lx_event!(
        "SHELL_RESIDENT_RECORD",
        "had the app started once more so that the dead shell's record is replaced",
        ok = status.is_ok_and(|s| s.success()),
        ms = begun.elapsed().as_millis()
    );
}

/// Called by a quit that keeps sessions, once the windows and the core
/// are gone.  Returns only when the exec failed; the caller then exits
/// as it used to.
pub(crate) fn enter(n_kept: usize) {
    lx_event!(
        "SHELL_RESIDENT_ENTER",
        "sessions are kept: staying, without a window, to answer for them",
        n_kept = n_kept
    );
    let err = become_image(&own_image(), true, &[]);
    lx_warn!("shell.resident.exec_failed", &format!("{err}; exiting, the sessions are left without an owner"));
}

/// Become the app again.  Does not return unless the exec failed.
fn resume(why: &'static str) {
    lx_event!("SHELL_RESIDENT_RESUME", "becoming the app again", why = why);
    let staged = [BinaryTree::for_shell(), BinaryTree::for_core()].into_iter().flatten().any(|t| t.has_pending())
        || marspot_term::paths::binaries_root().join("pending").join("marspot-session").exists();
    let seams = crate::instance::asked_with();
    let mut extra: Vec<(&str, &str)> = seams.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if staged {
        extra.push((UPDATE_WAITING_ENV, "1"));
    }
    let err = become_image(&own_image(), false, &extra);
    lx_warn!("shell.resident.resume_failed", &format!("{err}"));
}

/// Exit for real: nothing is left to answer for, or the login session
/// is ending.
fn leave(why: &'static str) -> ! {
    lx_event!("SHELL_RESIDENT_EXIT", "the resident shell is exiting", why = why);
    land_staged_bundle();
    std::process::exit(0)
}

/// Start the armed bundle lander, if there is one, and hand it the one
/// moment it can work in.
///
/// `install-local.sh` cannot write `Marspot.app/Contents/MacOS/` while
/// a process is running that binary: the system kills a process whose
/// file on disk stops matching what it was started from.  So a changed
/// shell is staged behind a one-shot `land.sh`, which waits for this
/// pid to be gone.  Its LaunchAgent at login is only a backstop: the
/// app is what the user opens first, so that copy finds it running and
/// gives up.
///
/// Detached into its own process group so the exit right after this
/// does not take it along.  Every failure is silent on purpose: nothing
/// about quitting may depend on an update landing.
pub(crate) fn land_staged_bundle() {
    let script = marspot_term::paths::state_root().join("pending-bundle").join("land.sh");
    if !script.exists() {
        return;
    }
    let pid = std::process::id();
    let spawned = std::process::Command::new(&script)
        .arg(pid.to_string())
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => lx_event!(
            "BUNDLE_LANDER_START",
            "handed the armed bundle lander this exit to land in",
            lander_pid = child.id(),
            waits_for = pid
        ),
        Err(e) => lx_warn!(
            "shell.quit.lander_spawn_failed",
            &format!("{e} — the stage stays armed for the login backstop")
        ),
    }
}

/// Take what an install staged while there is no core to take it.
///
/// A good shell is promoted and this process moves to it, still
/// resident.  A good core is promoted for the next one that is started.
/// A staged session binary stays where it is: the next core promotes it
/// and asks the sessions to move.
fn apply_staged() {
    let shell = BinaryTree::for_shell().ok().filter(|t| t.has_pending());
    let core = BinaryTree::for_core().ok().filter(|t| t.has_pending());
    for (layer, tree) in [("core", &core), ("shell", &shell)] {
        let Some(tree) = tree else { continue };
        if !marspot::binary_tree::can_start(&tree.pending()) {
            lx_event!("UPDATE_PROBE_REJECT", "staged binary could not start; quarantined", layer = layer);
            sup_log::log("UPDATE_PROBE_REJECT", &format!("{layer}: staged binary could not start"));
            let _ = tree.quarantine_pending();
            continue;
        }
        if let Err(e) = tree.promote_pending() {
            lx_warn!("shell.resident.promote_failed", &format!("{e}"), layer = layer);
            continue;
        }
        sup_log::log("RESIDENT_PROMOTED", &format!("pending/marspot-{layer} → current/"));
        if layer == "shell" {
            lx_event!("SHELL_UPDATE_APPLY", "moving the resident shell to the staged image");
            let err = become_image(&tree.current(), true, &[("MARSPOT_SHELL_SELF_UPDATE", "1")]);
            lx_warn!("shell.resident.update_exec_failed", &format!("{err}"));
        }
    }
}

/// Sleep until a session exits or an update is staged; exit with the
/// last session.
fn watch(mut sessions: Vec<i32>) {
    // SAFETY: plain kqueue calls on a queue made here; an all-zero
    // kevent is a valid one to fill in.
    unsafe {
        let kq = libc::kqueue();
        // not for the image this process becomes when the app is opened
        libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC);
        let add = |ident: usize, filter: i16, fflags: u32| {
            let ev = libc::kevent { ident, filter, flags: libc::EV_ADD, fflags, data: 0, udata: std::ptr::null_mut() };
            libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0
        };
        // ignored, so that the queue is the only thing it reaches
        libc::signal(libc::SIGUSR1, libc::SIG_IGN);
        add(libc::SIGUSR1 as usize, libc::EVFILT_SIGNAL, 0);
        // one that exited between the listing and here fails to add
        sessions.retain(|pid| add(*pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT));
        while !sessions.is_empty() {
            let mut ev: libc::kevent = std::mem::zeroed();
            if libc::kevent(kq, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) < 1 {
                continue;
            }
            if ev.filter == libc::EVFILT_PROC {
                sessions.retain(|pid| *pid != ev.ident as i32);
            } else {
                apply_staged();
            }
        }
    }
    leave("the last session ended");
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The application delegate is messaged on the main thread.
    // - No ivars and no Drop logic.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarspotResidentDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        // the Dock, Finder, `open`: LaunchServices finds this process
        // registered and sends it this instead of starting another
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _sender: &NSApplication, _visible: bool) -> bool {
            resume("the app was opened");
            false
        }

        // nothing in the interface can ask this of a process with no
        // Dock icon: it is the login session ending
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            leave("asked to quit")
        }
    }
);

/// Run as the resident.  The instance lock is already held.
pub(crate) fn run() -> ! {
    let started_by_a_session = std::env::var_os(RESIDENT_ENV).is_some_and(|v| v == marspot_term::owner::RESIDENT_STARTED);
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(RESIDENT_ENV) };
    launch::write_shell_pid();
    ownership::announce();
    let sessions = live_sessions();
    lx_event!(
        "SHELL_RESIDENT",
        "no window: answering for the sessions that are kept",
        pid = std::process::id(),
        n_sessions = sessions.len(),
        version_shell = env!("MARSPOT_VERSION_SHELL")
    );
    sup_log::log("SHELL_RESIDENT", &format!("pid={} sessions={}", std::process::id(), sessions.len()));
    ownership::log_sessions();
    if sessions.is_empty() {
        leave("no session is running");
    }
    if started_by_a_session || crate::instance::is_heir() {
        replace_dead_record();
    }
    crate::instance::serve_front(|| resume("a second launch asked"));
    if let Err(e) = std::thread::Builder::new().name("l1-resident".into()).spawn(move || watch(sessions)) {
        lx_warn!("shell.resident.watch_failed", &format!("{e}"));
        leave("cannot watch the sessions");
    }

    let mtm = MainThreadMarker::new().expect("the resident runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate: Retained<Delegate> = {
        let this = Delegate::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's `init`, on a freshly allocated subclass.
        unsafe { msg_send![super(this), init] }
    };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    leave("the run loop ended")
}

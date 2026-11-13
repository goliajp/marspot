//! The shell with no window: what stays of the app after the user quits
//! it while sessions run.
//!
//! Sessions outlive a quit, and they stay in the coalition of the shell
//! that started them.  LaunchServices holds this process answerable for
//! that coalition for as long as it is registered and alive; were it to
//! exit, the next program to register from a pane would have every
//! session listed as its own (`marspot_term::owner`).  So a quit with
//! sessions kept does not end the process.  The app's state is dropped,
//! its windows are closed, it leaves the Dock, and the same process goes
//! on with nothing but an event loop, asleep until one of three things
//! happens.  The user opens the app again, and this process replaces
//! its image with the app's, same pid, same coalition.  The last
//! session ends, and it exits for real.  An update is staged, and it
//! is taken on the way back to being the app.
//!
//! It stays the same image on purpose.  Replacing the image at the quit
//! made a far smaller process, and a window of a few hundred
//! milliseconds in which LaunchServices still sent this pid the event
//! of someone opening the app: the event was refused (error -600) or
//! accepted and lost, and the app did not open.  A process that has
//! taken the event may replace its image; one that is waiting for it
//! may not.  The shell a session starts with no window, after a crash,
//! has no earlier image to lose an event.

use std::os::unix::process::CommandExt;

use marspot::{lx_event, lx_warn};
use marspot_term::owner::{HEIR_ENV, RESIDENT_ENV};
use marspot_term::session_registry;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSApplicationTerminateReply,
    NSRunningApplication,
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

/// Replace this image with `exe`, same pid and coalition, as the app.
/// Returns only when the exec failed.
fn become_image(exe: &std::path::Path, extra: &[(&str, &str)]) -> std::io::Error {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut cmd = std::process::Command::new(exe);
    if let Some(arg0) = args.first() {
        cmd.arg0(arg0);
    }
    cmd.args(args.iter().skip(1)).env_remove(RESIDENT_ENV).env_remove(HEIR_ENV);
    // the dev seams were for the app that was put away; the mark of a
    // sandbox is not one of them
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|n| n.starts_with("MARSPOT_DEV_") && n != "MARSPOT_DEV_SANDBOX") {
            cmd.env_remove(name);
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

/// Be an instance of the app for as long as it takes to register, and
/// exit, touching nothing.
pub(crate) fn ls_refresh_main() -> ! {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        app.finishLaunching();
    }
    std::process::exit(0)
}

/// For a shell a session started, after the shell died or for one
/// that came up outside the coalition: have LaunchServices start the
/// bundle once more, and return when that instance has registered and
/// exited, for this process to register right after it.
///
/// The dead shell's record stays, listing the coalition's members,
/// until the app is started again; then the coalition is nobody's, and
/// the first program to register from inside it has the members listed
/// as its own.  Started from here that moment is ours.  Measured on
/// macOS 27.2: without this the members stay on the dead record, and
/// the user opening the app is what frees them; an instance that exits
/// without registering changes nothing; a process that registered
/// before the dead record went is not given the members afterwards.
///
/// The order is what makes it work.  LaunchServices has to have seen
/// the dead shell exit before the instance is started: under load it
/// sees that as much as 0.3 s late, an instance that registers before
/// then replaces nothing, and this process is then given the members
/// as a related coalition, which does not keep a program registering
/// from a pane from being given them too.  So this waits until no
/// record of this app is left for a process that is gone, starts the
/// instance, and waits for the same again before it registers.
///
/// Between the dead record going and this process registering the
/// coalition is nobody's: some tens of milliseconds, which a program
/// registering from a pane in that moment does win.
pub(crate) fn replace_dead_record() {
    let exe = ownership::successor_exe();
    let Some(bundle) = ownership::bundle_of(&exe) else { return };
    let patience = std::time::Duration::from_secs(2);
    let asked = std::time::Instant::now();
    let seen = records_of_the_dead_are_gone(patience);
    let begun = std::time::Instant::now();
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-n", "-g", "-j", "-W"])
        .arg(bundle)
        .args(["--args", LS_REFRESH_ARG])
        .stdin(std::process::Stdio::null())
        .status();
    let started = begun.elapsed();
    let gone = records_of_the_dead_are_gone(patience);
    lx_event!(
        "SHELL_RESIDENT_RECORD",
        "had the app started once more so that the dead shell's record is replaced",
        ok = status.is_ok_and(|s| s.success()),
        ms = started.as_millis(),
        exit_seen = seen,
        exit_seen_ms = (begun - asked).as_millis(),
        dead_records_gone = gone,
        waited_ms = (begun.elapsed() - started).as_millis()
    );
}

/// Wait until LaunchServices lists no record of this app whose process
/// no longer exists.  Asking does not register this process.
fn records_of_the_dead_are_gone(patience: std::time::Duration) -> bool {
    let Some(id) = objc2_foundation::NSBundle::mainBundle().bundleIdentifier() else { return false };
    let deadline = std::time::Instant::now() + patience;
    loop {
        let left = objc2::rc::autoreleasepool(|_| {
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&id)
                .iter()
                // SAFETY: signal 0 only asks whether the pid exists.
                .filter(|app| unsafe { libc::kill(app.processIdentifier(), 0) } != 0)
                .count()
        });
        if left == 0 {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Set by a quit that keeps sessions: when the app's event loop has
/// returned, this process stays as the resident.
static STAYING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called by a quit that keeps sessions, before it stops the app's
/// event loop.
pub(crate) fn stay(n_kept: usize) {
    lx_event!(
        "SHELL_RESIDENT_ENTER",
        "sessions are kept: staying, without a window, to answer for them",
        n_kept = n_kept
    );
    STAYING.store(true, std::sync::atomic::Ordering::Release);
}

pub(crate) fn staying() -> bool {
    STAYING.load(std::sync::atomic::Ordering::Acquire)
}

/// How long opening the app waits for a staged shell to prove that it
/// starts before it opens as the shell it is.
const STAGED_SHELL_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// Become the app again: as the staged shell when there is one that
/// starts, as this image otherwise.  Does not return unless the exec
/// failed.
///
/// This is the one moment a resident replaces its image, and the event
/// of the app being opened is already in hand.
fn resume(why: &'static str) {
    lx_event!("SHELL_RESIDENT_RESUME", "becoming the app again", why = why);
    let seams = crate::instance::asked_with();
    let mut extra: Vec<(&str, &str)> = seams.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut image = own_image();
    let bundle_dir = image.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    if let Ok(tree) = BinaryTree::for_shell()
        && tree.has_pending()
    {
        if !marspot::binary_tree::can_start_within(&tree.pending(), STAGED_SHELL_PATIENCE) {
            lx_event!("UPDATE_PROBE_REJECT", "staged binary could not start; quarantined", layer = "shell");
            sup_log::log("UPDATE_PROBE_REJECT", "shell: staged binary could not start");
            let _ = tree.quarantine_pending();
        } else if let Err(e) = tree.promote_pending() {
            lx_warn!("shell.resident.promote_failed", &format!("{e}"), layer = "shell");
        } else {
            lx_event!("SHELL_UPDATE_APPLY", "opening as the staged shell");
            sup_log::log("SHELL_UPDATE_APPLY", "promoting pending/marspot-shell → current/ on the way back");
            image = tree.current();
            extra.push(("MARSPOT_SHELL_SELF_UPDATE", "1"));
            extra.push(("MARSPOT_NO_REDIRECT", "1"));
            // the shell it moves to finds the bundle through this; one
            // that moved before keeps what it was given
            if std::env::var_os("MARSPOT_BUNDLE_DIR").is_none() {
                extra.push(("MARSPOT_BUNDLE_DIR", bundle_dir.to_str().unwrap_or_default()));
            }
        }
    }
    let staged = BinaryTree::for_core().is_ok_and(|t| t.has_pending())
        || marspot_term::paths::binaries_root().join("pending").join("marspot-session").exists();
    if staged {
        extra.push((UPDATE_WAITING_ENV, "1"));
    }
    let err = become_image(&image, &extra);
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
/// A good core is promoted for the next one that is started.  A staged
/// shell waits for the app to be opened (`resume`): a resident does not
/// replace its image while it waits for that event.  It is run once
/// here so that the system's first look at the new file is paid now.
/// A staged session binary stays where it is: the next core promotes
/// it and asks the sessions to move.
fn apply_staged() {
    if let Ok(core) = BinaryTree::for_core()
        && core.has_pending()
    {
        if !marspot::binary_tree::can_start(&core.pending()) {
            lx_event!("UPDATE_PROBE_REJECT", "staged binary could not start; quarantined", layer = "core");
            sup_log::log("UPDATE_PROBE_REJECT", "core: staged binary could not start");
            let _ = core.quarantine_pending();
        } else if let Err(e) = core.promote_pending() {
            lx_warn!("shell.resident.promote_failed", &format!("{e}"), layer = "core");
        } else {
            sup_log::log("RESIDENT_PROMOTED", "pending/marspot-core → current/");
        }
    }
    if let Ok(shell) = BinaryTree::for_shell()
        && shell.has_pending()
    {
        let starts = marspot::binary_tree::can_start(&shell.pending());
        lx_event!("SHELL_UPDATE_WAITS", "a staged shell is taken when the app is opened", starts = starts);
        sup_log::log("SHELL_UPDATE_WAITS", "pending/marspot-shell is taken when the app is opened");
        if !starts {
            let _ = shell.quarantine_pending();
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

fn announce_and_list() -> Vec<i32> {
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
    sessions
}

/// Watch the sessions and wait for the app to be opened.
fn wait(sessions: Vec<i32>, app: &NSApplication, mtm: MainThreadMarker) -> ! {
    if let Err(e) = std::thread::Builder::new().name("l1-resident".into()).spawn(move || watch(sessions)) {
        lx_warn!("shell.resident.watch_failed", &format!("{e}"));
        leave("cannot watch the sessions");
    }
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    lx_event!("SHELL_RESIDENT_REGISTERED", "registered with no Dock icon");
    let delegate: Retained<Delegate> = {
        let this = Delegate::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's `init`, on a freshly allocated subclass.
        unsafe { msg_send![super(this), init] }
    };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    leave("the run loop ended")
}

/// Run as the resident in an image started for it: a shell a session
/// started.  The instance lock is already held.
pub(crate) fn run() -> ! {
    let started_by_a_session = std::env::var_os(RESIDENT_ENV).is_some_and(|v| v == marspot_term::owner::RESIDENT_STARTED);
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(RESIDENT_ENV) };
    let sessions = announce_and_list();
    if started_by_a_session || crate::instance::is_heir() {
        replace_dead_record();
    }
    crate::instance::serve_front(|| resume("a second launch asked"));
    let mtm = MainThreadMarker::new().expect("the resident runs on the main thread");
    wait(sessions, &NSApplication::sharedApplication(mtm), mtm)
}

/// This process's physical footprint in KiB, as Activity Monitor counts it.
fn footprint_kib() -> u64 {
    // SAFETY: the kernel fills the struct or fails; all-zero is valid.
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast());
        info.ri_phys_footprint / 1024
    }
}

/// Stay as the resident in the image that was the app, after its event
/// loop returned and its state was dropped.
pub(crate) fn run_in_place() -> ! {
    let sessions = announce_and_list();
    // a launch that stands aside now asks a resident: `resume`
    crate::instance::answer_as_resident(|| resume("a second launch asked"));
    let mtm = MainThreadMarker::new().expect("the resident runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    crate::theme_follow::put_away();
    let before = footprint_kib();
    // in a pool of its own: the list of windows is what keeps them, and
    // their backing stores, until the pool it was made in is drained --
    // which the pool `main` runs in never is
    objc2::rc::autoreleasepool(|_| {
        for window in app.windows().iter() {
            window.setDelegate(None);
            window.close();
        }
        app.setMainMenu(None);
        // SAFETY: nil restores the icon the bundle names; the image set at
        // launch is a megabyte this process has no Dock tile to show in.
        unsafe { app.setApplicationIconImage(None) };
    });
    lx_event!(
        "SHELL_RESIDENT_MEMORY",
        "the app's state is dropped and its windows are closed",
        footprint_kib_before = before,
        footprint_kib = footprint_kib()
    );
    wait(sessions, &app, mtm)
}

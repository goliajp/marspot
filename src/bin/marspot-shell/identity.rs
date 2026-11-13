//! What LaunchServices takes this process for, and what it lists
//! under it.
//!
//! The app has to be one record there: named after the bundle, alive,
//! and the one that is held answerable for the sessions' coalition
//! (`marspot_term::owner`).  What LaunchServices does decides how that
//! is kept; all of it measured on macOS 27.2.1, none of it documented:
//!
//! - A process it did not start is listed by the file it runs when it
//!   first registers.  One started from a copy of the shell outside the
//!   bundle is a second application, named after the file, with a Dock
//!   tile of its own; opening the app starts another beside it.  When
//!   such a process replaces its image with the bundle's executable,
//!   it is listed as the bundle from then on, and keeps its coalition.
//! - A coalition is given to a record when the record is made, and
//!   only when no other record has it, of a living process or a dead
//!   one.  A process that registered while another record had it is
//!   not given it later: only a new process can be.
//! - A record of the app whose process is gone stays for as long as a
//!   process is left in a coalition it has or was handed.  A process of
//!   the app that registers, or registers again after replacing its
//!   image, takes over one such record at a time, if it was started by
//!   the same path, as text, as the dead one: the dead record goes, and
//!   what it listed becomes related coalitions of the one that took it
//!   over, which keep no stranger out.  An instance that takes one over
//!   and then exits is the next dead record.

use std::os::unix::process::CommandExt;
use std::path::Path;

use marspot::{lx_event, lx_warn};
use marspot_term::platform;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSRunningApplication};

use crate::ls_records::{self, Holder};
use crate::ownership;

/// What this process is in the middle of, for the sake of its record,
/// from one image to the next.
const PASS_ENV: &str = "MARSPOT_LS_PASS";
/// It became the bundle's executable to be listed under its name.
const NAMED: &str = "named";
/// Dead records of the app are left: it registers, which takes one
/// over, and looks again.  Followed by how many more times it may.
const TAKING_OVER: &str = "taking-over:";
/// Nothing is left to do.
const SETTLED: &str = "settled";

/// Instances started, or images replaced, before giving up on a state
/// that does not change.
const TRIES: u32 = 4;

static RENAMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process was listed outside the bundle until its last
/// image.  It is the app by name now, and cannot be given the
/// coalition it is in: `ownership::hand_over_if_outside` has a new
/// shell started in its place.
pub(crate) fn renamed() -> bool {
    RENAMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Replace this image with `exe`: same pid, same arguments, same
/// environment but for the pass.  Returns only when the exec failed.
fn become_image(exe: &Path, pass: &str, extra: &[(&str, &str)]) -> std::io::Error {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut cmd = std::process::Command::new(exe);
    cmd.arg0(exe).args(args.iter().skip(1)).env(PASS_ENV, pass);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    crate::instance::carry_lock(&mut cmd);
    crate::reclaim::carry_count(&mut cmd);
    let err = cmd.exec();
    crate::instance::keep_lock_here();
    err
}

/// This image's file, by the path LaunchServices knows the app's
/// instances by (`owner::resolved`).
fn own_exe() -> std::path::PathBuf {
    marspot_term::owner::resolved(&std::env::current_exe().unwrap_or_default())
}

pub(crate) fn bundle_id() -> Option<String> {
    objc2_foundation::NSBundle::mainBundle().bundleIdentifier().map(|id| id.to_string())
}

/// Whether LaunchServices lists this process, and under `bundle`.
/// Asking does not register this process.
fn listed_under(bundle: &Path) -> Option<bool> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(std::process::id() as libc::pid_t)?;
    let listed = app.bundleURL().and_then(|url| url.path()).map(|p| std::path::PathBuf::from(p.to_string()));
    let same = |a: &Path, b: &Path| a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b);
    Some(app.bundleIdentifier().is_some() && listed.is_some_and(|l| same(&l, bundle)))
}

/// Register this process with no window and no Dock icon.
fn register() -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    true
}

/// With this process registered: go on as a new image.  While a dead
/// record of the app is listed, the next image is one that only
/// registers, which takes one over, and comes back here.  Which one
/// goes is not ours to choose, and a dead record can be one that
/// `lsappinfo` no longer lists: this looks again each time.
fn go_on(left: u32, extra: &[(&str, &str)]) {
    let dead = bundle_id().map_or(0, |id| ls_records::our_dead(&ls_records::list(), &id));
    let pass = if dead > 0 && left > 0 { format!("{TAKING_OVER}{}", left - 1) } else { SETTLED.to_string() };
    let err = become_image(&own_exe(), &pass, extra);
    lx_warn!("shell.identity.exec_failed", &format!("{err}"));
}

/// Before the instance lock is taken: get this process listed as the
/// app, if an earlier image of it was listed as something else.
///
/// A shell that a session started from a copy outside the bundle is
/// listed under the copy's name.  It becomes the bundle's executable
/// here, when that is the same build as this image.  The window is
/// already gone at this point, as in any update of the shell.
pub(crate) fn settle() {
    let pass = std::env::var(PASS_ENV).unwrap_or_default();
    crate::reclaim::take_count(!pass.is_empty());
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(PASS_ENV) };
    if pass == NAMED {
        RENAMED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(left) = pass.strip_prefix(TAKING_OVER)
        && register()
    {
        go_on(left.parse().unwrap_or(0), &[]);
    }
    // an image that is the bundle's executable is listed as the app,
    // however this process was first listed: nothing to ask
    if !pass.is_empty() || ownership::bundle_of(&own_exe()).is_some() {
        return;
    }
    let shell = ownership::successor_exe();
    let Some(bundle) = ownership::bundle_of(&shell) else { return };
    if listed_under(bundle) != Some(false) {
        return;
    }
    let build = marspot_term::build::of_file(&shell);
    if build.is_none() || build != marspot_term::build::of_file(&own_exe()) {
        return lx_event!(
            "SHELL_IDENTITY_KEPT",
            "listed outside the bundle, and the bundle's shell is another build: staying as listed",
            bundle_shell = shell.display()
        );
    }
    lx_event!(
        "SHELL_IDENTITY",
        "listed outside the bundle: becoming the bundle's executable to be listed as the app",
        was = own_exe().display(),
        becomes = shell.display()
    );
    let err = become_image(&shell, NAMED, &[]);
    lx_warn!("shell.identity.exec_failed", &format!("{err}"));
}

/// For a shell that goes on as the app, once it is on record: an heir
/// takes the coalition first (`take_coalition_as`; `staged` says that
/// it has an update to finish), and any shell sees to it that it has
/// the coalition it is in.
pub(crate) fn take_coalition(staged: bool) {
    if crate::instance::is_heir() {
        take_coalition_as(if staged { &[(crate::resident::UPDATE_WAITING_ENV, "1")] } else { &[] });
    }
    crate::reclaim::watch();
}

/// The argument of the instance `replace_dead_record` starts.
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

/// For a new shell that takes the place of one that is gone, started
/// in the sessions' coalition and not yet registered: get the
/// coalition listed under this process, and leave no dead record of
/// the app.  Returns when it could not replace its image, or has no
/// bundle; the caller goes on as the shell it is.  `extra` is set for
/// the image that goes on.
///
/// While a dead record of the app has the coalition, an instance of
/// the bundle is started through LaunchServices to take a dead record
/// over.  It takes one, not always the one with the coalition, so this
/// is repeated until that one has gone.  Then the coalition is
/// nobody's, this process registers, and has it.  The instances have
/// left dead records of their own; this process registering, and
/// replacing its image as many times as there are more, takes them
/// over.
///
/// Registering while the dead record still has the coalition is the
/// one thing not to do: this process would take the record over and
/// not be given the coalition, which is then nobody's.
///
/// Between the dead record going and this process registering the
/// coalition is nobody's: some tens of milliseconds, which a program
/// registering from a pane in that moment does win.  Nor is anything
/// done here about a coalition listed under another program's record.
pub(crate) fn take_coalition_as(extra: &[(&str, &str)]) {
    let exe = ownership::successor_exe();
    let (Some(bundle), Some(id), Some(coalition)) =
        (ownership::bundle_of(&exe), bundle_id(), platform::coalition_of(std::process::id() as i32))
    else {
        return;
    };
    let me = std::process::id() as i32;
    let began = std::time::Instant::now();
    // LaunchServices sees an exit as much as 0.3 s late under load, and
    // an instance that registers before then takes nothing over
    let exit_seen = records_of_the_dead_are_gone(&id);
    let mut records = ls_records::list();
    let mut instances = 0;
    while ls_records::holder_of(&records, coalition, me, &id) == Holder::OurDead && instances < TRIES {
        let alone = ls_records::our_dead(&records, &id) == 1;
        replace_dead_record(bundle, &id);
        instances += 1;
        // the only dead record there was is the one that went: the
        // time a look would take is time the coalition is nobody's
        if alone {
            break;
        }
        records = ls_records::list();
    }
    let cleared = std::time::Instant::now();
    register();
    let nobodys = cleared.elapsed();
    let holder = ls_records::holder_of(&ls_records::list(), coalition, me, &id);
    lx_event!(
        "SHELL_COALITION_TAKEN",
        "registered in the sessions' coalition",
        coalition = coalition,
        listed_under = format!("{holder:?}"),
        instances = instances,
        exit_seen = exit_seen,
        ms = (cleared - began).as_millis(),
        // from the last look or instance to this process being registered
        registered_after_ms = nobodys.as_millis()
    );
    go_on(TRIES, extra);
}

/// Have LaunchServices start the bundle once more, and return when
/// that instance has registered and exited.  An instance that exits
/// without registering changes nothing.
fn replace_dead_record(bundle: &Path, id: &str) {
    let begun = std::time::Instant::now();
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-n", "-g", "-j", "-W"])
        .arg(bundle)
        .args(["--args", LS_REFRESH_ARG])
        .stdin(std::process::Stdio::null())
        .status();
    let started = begun.elapsed();
    let gone = records_of_the_dead_are_gone(id);
    lx_event!(
        "SHELL_RESIDENT_RECORD",
        "had the app started once more so that a dead record of it is taken over",
        ok = status.is_ok_and(|s| s.success()),
        ms = started.as_millis(),
        dead_records_gone = gone,
        waited_ms = (begun.elapsed() - started).as_millis()
    );
}

/// Wait, for two seconds at most, until LaunchServices lists no
/// running instance of this app whose process no longer exists.
/// Asking does not register this process.
fn records_of_the_dead_are_gone(id: &str) -> bool {
    let id = objc2_foundation::NSString::from_str(id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
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

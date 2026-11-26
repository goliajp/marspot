//! Who ends this image: a resume, a leave, or a replacement for a lost
//! Apple event registration, one at a time.

use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};

use marspot::lx_event;
use marspot_term::owner::{HEIR_ENV, RESIDENT_ENV};

use super::own_image;

/// Set by whichever of a resume, a leave and a replacement for a lost
/// Apple event registration goes first: the first two run on the main
/// thread or on the launch listener's, the last on the check's thread,
/// and two of them at once would exec twice, or exec while the bundle
/// lander is handed this pid.  Cleared only by a failed exec.
static ENDING_IMAGE: AtomicBool = AtomicBool::new(false);

fn claim_image() -> bool {
    ENDING_IMAGE.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Wait for the claim: whoever holds it is an exec or an exit away, so
/// this thread either goes with the image or gets the claim when the
/// exec failed.
pub(super) fn wait_for_image() {
    while !claim_image() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Replace this image with itself as a resident, for a lost Apple event
/// registration, unless a resume or a leave is already ending it: a
/// resumed image checks its registration anew.  `None` when skipped.
pub(crate) fn replace_as_resident(extra: &[(&str, &str)]) -> Option<std::io::Error> {
    if !claim_image() {
        lx_event!("SHELL_AE_REPLACE_SKIPPED", "the resident is already becoming another image");
        return None;
    }
    Some(become_image(&own_image(), extra))
}

/// Replace this image with `exe`, same pid and coalition, as the app.
/// The caller holds the claim; returns, giving it back, only when the
/// exec failed.
pub(super) fn become_image(exe: &std::path::Path, extra: &[(&str, &str)]) -> std::io::Error {
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
    crate::reclaim::carry_look(&mut cmd);
    crate::apple_events::mark(&mut cmd);
    let err = crate::platform::exec_hooks::exec_prepared(marspot_term::exec_contract::ExecKind::ShellResident, &mut cmd);
    crate::instance::keep_lock_here();
    ENDING_IMAGE.store(false, Ordering::SeqCst);
    err
}

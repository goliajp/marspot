//! The shell replacing its own image: with a staged update, or with
//! itself when appleeventsd lost its registration (see `apple_events`).

use marspot::{lx_error, lx_event, lx_warn};

use crate::platform::exec_hooks;
use crate::plugins::peerbus;
use crate::{apple_events, sup_log, supervisor, ShellApp};

impl ShellApp {
    /// Task C — shell self-update.  Detect a pending shell binary,
    /// promote it into `current/`, and `execv` over ourselves so the
    /// new shell binary takes over the same process slot.  The window
    /// flashes closed → open in ~100 ms; the new shell reconnects to
    /// shelld and reattaches the user's existing sessions so terminal
    /// content survives.
    ///
    /// Returns `true` if exec was attempted (caller's stack is gone
    /// past that point, but Rust can't express it).  Returns `false`
    /// if no pending shell was found *or* if any prep step failed.
    ///
    /// Reached only after `start_probe(ProbeLayer::Shell, …)` came back
    /// good, which is what makes the promotion safe: `exec` replaces
    /// this process, so an image that cannot run means the app is
    /// simply gone — no window, no log line (the redirect at the top of
    /// `main` runs before `logx::init`), every live session orphaned.
    /// That is exactly what happened on 2026-07-26, when an adhoc-signed
    /// binary reached `pending/` and AMFI killed the successor.
    pub(super) fn finish_shell_self_update(&mut self, window_frame_pt: (f64, f64, f64, f64)) -> bool {
        use std::os::unix::process::CommandExt;
        let shell_tree = match supervisor::BinaryTree::for_shell() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if !shell_tree.has_pending() {
            return false;
        }
        lx_event!(
            "SHELL_UPDATE_APPLY",
            "applying pending shell self-update"
        );
        sup_log::log(
            "SHELL_UPDATE_APPLY",
            "promoting pending/marspot-shell → current/",
        );
        if let Err(e) = shell_tree.promote_pending() {
            lx_event!(
                "SHELL_UPDATE_FAIL",
                "shell promote_pending failed",
                error = format!("{e}")
            );
            sup_log::log("SHELL_UPDATE_FAIL", &format!("promote: {e}"));
            return false;
        }
        // Tear down what we can pre-exec so the new shell starts
        // fresh.  Core child gets killed; control socket is dropped;
        // IOSurface is released.  Anything left would be inherited as
        // dangling fds in the new process — clean now.
        self.shutdown_active("shell self-update execv prep");
        // Every window's surfaces, not just one: anything left behind
        // is inherited by the new image as a dangling fd.
        for w in &mut self.windows {
            if let Some(s) = w.surfaces.take() {
                s.release();
            }
            if let Some(s) = w.pending_surfaces.take() {
                s.release();
            }
        }
        let target = shell_tree.current();
        if !target.exists() {
            lx_event!(
                "SHELL_UPDATE_FAIL",
                "post-promote current/marspot-shell missing — aborting exec"
            );
            sup_log::log("SHELL_UPDATE_FAIL", "post-promote current missing");
            return false;
        }
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let arg0 = args.first().cloned().unwrap_or_else(|| target.clone().into());
        // Pass the bundle directory so the new shell can fall back
        // there when looking for marspot-core (the updater might
        // have staged only shell, not core).  Computed from our own
        // `current_exe()` parent.
        let bundle_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        // Pass a marker so the new shell logs SHELL_SELF_UPDATE on
        // startup and so the redirect-loop guard doesn't fire (the
        // new binary IS the one we want to run).
        // Hand the live window frame to the successor so its window
        // opens exactly in place (no jump back to the default rect).
        let (fx, fy, fw, fh) = window_frame_pt;
        let mut cmd = std::process::Command::new(&target);
        cmd.arg0(arg0)
            .args(args.iter().skip(1))
            .env("MARSPOT_NO_REDIRECT", "1")
            .env("MARSPOT_SHELL_SELF_UPDATE", "1")
            .env("MARSPOT_BUNDLE_DIR", &bundle_dir)
            .env("MARSPOT_RESTORE_FRAME", format!("{fx},{fy},{fw},{fh}"));
        apple_events::mark(&mut cmd);
        let err = exec_prepared(&mut cmd);
        lx_event!(
            "SHELL_UPDATE_FAIL",
            "exec into current/marspot-shell failed",
            target = target.display(),
            error = format!("{err}")
        );
        sup_log::log("SHELL_UPDATE_FAIL", &format!("exec: {err}"));
        false
    }

    /// Replace this image with itself, because appleeventsd lost its
    /// registration in the last exec (see `apple_events`); `n` counts
    /// such replacements in a row.  The window flashes as in an update.
    pub(super) fn replace_image_for_apple_events(&mut self, n: u32) {
        use std::os::unix::process::CommandExt;
        let target = match std::env::current_exe() {
            Ok(t) => t,
            Err(e) => {
                lx_warn!("shell.ae_check.no_exe", &e.to_string());
                return apple_events::let_present(Some(&self.proxy));
            }
        };
        lx_event!(
            "SHELL_AE_REPLACE",
            "replacing the image again so that it registers for Apple events",
            replacement = n
        );
        let frame = self.windows.first().and_then(|w| w.last_saved_window).map(|(x, y, w, h, _)| (x, y, w, h));
        self.shutdown_active("apple events re-registration execv prep");
        for w in &mut self.windows {
            if let Some(s) = w.surfaces.take() {
                s.release();
            }
            if let Some(s) = w.pending_surfaces.take() {
                s.release();
            }
        }
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let mut cmd = std::process::Command::new(&target);
        if let Some(arg0) = args.first() {
            cmd.arg0(arg0);
        }
        cmd.args(args.iter().skip(1)).env("MARSPOT_NO_REDIRECT", "1");
        if let Some((fx, fy, fw, fh)) = frame {
            cmd.env("MARSPOT_RESTORE_FRAME", format!("{fx},{fy},{fw},{fh}"));
        }
        apple_events::mark_retry(&mut cmd, n);
        let err = exec_prepared(&mut cmd);
        lx_error!("shell.ae_check.exec_failed", &format!("{err}"), target = target.display());
        // the frames held were of surfaces released above; the next core
        // brings new ones, and they must not be held for a check that
        // will not come
        for w in &mut self.windows {
            w.held_frame = None;
        }
        apple_events::let_present(Some(&self.proxy));
    }
}

/// Exec `cmd` as this process's next image, after what has to end
/// before it; both ways the shell replaces itself go through here.
/// Returns only when the exec failed.
pub(crate) fn exec_prepared(cmd: &mut std::process::Command) -> std::io::Error {
    // a relay left open ends after the exec as a child nobody waits for
    peerbus::close_inboxes();
    // a forced kill still waiting would be dropped unrecorded
    crate::plugins::zombie::stop::force::abandon();
    let err = exec_hooks::exec_prepared(cmd);
    peerbus::exec_failed();
    err
}

/// Whether the image before this one was the shell applying an update;
/// the mark leaves the environment.  Before any thread is started.
pub(crate) fn arrived_by_update() -> bool {
    if std::env::var_os("MARSPOT_SHELL_SELF_UPDATE").is_none() {
        return false;
    }
    // Take it out of the environment before anything else is
    // spawned.  Children inherit it, and every child that runs
    // this binary logged the line again: 38 of them in a day
    // against 6 real updates, one a minute, and the log read as if
    // L1 were re-execing itself all day (2026-09-28 — it sent this
    // author chasing a self-update that never happened).
    // SAFETY: single-threaded, before any thread is started.
    unsafe { std::env::remove_var("MARSPOT_SHELL_SELF_UPDATE") };
    sup_log::log("SHELL_SELF_UPDATE", "new shell exec'd from current/");
    // Self-fire SIGUSR1 so the main loop, once it lands on Idle
    // after the post-execv boot dance (active L2 spawned +
    // HELLO_ACK + first SurfaceReady), proactively applies any
    // remaining pending/marspot-core + pending/marspot-session.
    // Without this, the just-finished L1 swap leaves L2/L3
    // pending/ sitting there until install-local's 8 s fallback
    // retrigger or the next user-initiated focus-loss — that's
    // the "L1 flashes, then several seconds before content
    // updates and input wakes up" wait the user sees.
    // (install-local DID stage all three together as one
    // operation; the post-execv shell should treat them as one
    // operation too.)
    true
}

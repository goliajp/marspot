//! A cold start that finds the state a previous life of this session
//! saved: the pane comes back with its history.

use marspot_term::{lx_event, lx_warn};

use crate::local_session::LocalSession;
use crate::session_state_bin_path;

pub(crate) fn cold_snapshot(id: u64, local: &mut LocalSession) {
    // Cold-start resurrection:if a state.bin exists for this
    // session id,it's the dump a previous-life L3 wrote on
    // SIGTERM clean-exit(user closed window).Apply it to the
    // fresh Terminal so the user sees their saved scrollback
    // when they reopen the app.PTY child is brand-new, will
    // re-render its prompt over the saved screen — the
    // scrollback survives untouched (only the visible window
    // gets overwritten by the new shell's first paint).
    let state_path = session_state_bin_path(id);
    if state_path.exists() {
        match std::fs::read(&state_path) {
            Ok(body) => {
                let body_bytes = body.len();
                match local.terminal_mut().apply_snapshot(&body) {
                    Ok(()) => {
                        // The snapshot's *content* is what we
                        // want; its *modes* belong to a process
                        // that no longer exists.  This shell is
                        // brand-new and has switched nothing on.
                        //
                        // Leaving them set is user-visible: a
                        // restored session whose old TUI had
                        // mouse tracking on turns every scroll
                        // into `CSI < 64;x;y M` typed at the
                        // zsh prompt — "command not found:
                        // 29M64", over and over.
                        local.terminal_mut().reset_process_owned_modes();
                        lx_event!(
                            "L3_COLD_SNAPSHOT_APPLIED",
                            "restored Terminal from state.bin at cold boot — close→reopen resurrection",
                            session_id = id,
                            body_bytes = body_bytes
                        );
                    }
                    Err(e) => {
                        lx_warn!(
                            "l3.cold.snapshot_apply_failed",
                            &format!("{e}"),
                            body_bytes = body_bytes
                        );
                    }
                }
            }
            Err(e) => {
                lx_warn!(
                    "l3.cold.snapshot_read_failed",
                    &format!("{e}"),
                    path = state_path.display()
                );
            }
        }
        // state.bin is one-shot — drop so a future SIGKILL doesn't
        // resurrect stale state on top of the new shell's real output.
        let _ = std::fs::remove_file(&state_path);
    }
}

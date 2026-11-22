//! One live core process as the shell holds it: the child, the control
//! socket and the liveness handshake.

use super::*;

/// Everything tied to one live core process: its child handle, the
/// control socket (both directions), and the liveness-handshake
/// bookkeeping.  Aggregating these is what makes a flash-free silent
/// update possible — the shell can hold an `active` and a `pending`
/// CoreConn at once, let a fresh core prove itself, and only then
/// atomic-swap the presenter and tear the old one down.
///
/// `child` is `Option` because the child can legitimately be gone
/// while the rest of the conn lives: after a HELLO mismatch we kill
/// the child but keep `spawned_at` so the HELLO-timeout path in
/// `poll_supervisor` respawns; and `poll_supervisor` reaps the child
/// (`child.take()`) before `restart_core` drops the whole conn.
pub(super) struct CoreConn {
    /// The core child process, or `None` once killed/reaped.
    pub(super) child: Option<Child>,
    /// Parent end of the AF_UNIX socketpair we share with the core.
    /// Wrapped in `Mutex` so the `MarspotApp` callbacks (all on the
    /// main thread, but the type system doesn't know that) can mutate
    /// it without splitting the struct.  Frames written here arrive
    /// at the core's stdin-side fd 3 / `MARSPOT_SHELL_CONTROL_FD`.
    pub(super) control_tx: Mutex<UnixStream>,
    /// Receives parsed inbound frames from this core's reader thread.
    pub(super) control_rx: Receiver<ShellInbox>,
    /// True after this core has confirmed at least one HelloAck.
    pub(super) hello_acked: bool,
    /// The core answered `FramePace`: it waits for `FrameShown`, so its
    /// surfaces go on the window as they are.  Settled by the HelloAck,
    /// which such a core sends after that answer.
    pub(super) paced: bool,
    /// Surfaces announced before the HelloAck said how to show them.
    pub(super) early_ready: Vec<u32>,
    /// When this core was spawned — drives the HELLO timeout.
    pub(super) spawned_at: Instant,
    /// When to fire the next PING to this core.
    pub(super) next_ping_at: Instant,
    /// Nonce of the most recent PING we sent this core.  Pongs with a
    /// different nonce are stale and ignored.  Per-conn: a fresh core
    /// gets its own channel, so a previous core's Pong can't reach it.
    pub(super) last_ping_nonce: u32,
    /// When the most recent matching Pong arrived — or, until the
    /// first one does, when the core became **able** to answer at all
    /// (its HelloAck).  Seeding this at spawn instead is what turned
    /// one slow boot into a bricked window: see the reset in the
    /// `HelloAck` arm.
    pub(super) last_pong_at: Instant,
}

impl CoreConn {
    /// Write a frame to this core.  Logs and drops on EPIPE; a core
    /// dying mid-session is the supervisor's problem, not this path's.
    /// True when the frame was written.
    pub(super) fn send(&self, msg_type: MsgType, payload: Vec<u8>) -> bool {
        let frame = Frame::new(msg_type, payload);
        let mut stream = match self.control_tx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: still try
        };
        if let Err(e) = frame.write_to(&mut *stream) {
            lx_warn!(
                "shell.control_frame_write_failed",
                &format!("{e}"),
                msg_type = format!("{:?}", msg_type)
            );
            return false;
        }
        true
    }

    /// Drop the control socket (clean EOF for the core's read side),
    /// then SIGKILL + reap the child.  Order matters: the EOF gives a
    /// well-behaved core the chance to exit on its own before the kill.
    pub(super) fn shutdown(self) {
        let CoreConn { child, control_tx, control_rx, .. } = self;
        drop(control_tx);
        drop(control_rx);
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

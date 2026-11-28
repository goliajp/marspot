//! Naming and finding panes for the CLI, and reading from or typing into
//! one on its behalf.

use super::*;

impl ShellApp {
    /// Every pane in `obs` as `(session id, cwd, title)`.
    pub(super) fn live_panes(obs: &observe::Observation) -> Vec<(u64, String, String)> {
        pane_control::names::live_panes(obs)
    }

    fn pane_refs() -> Vec<marspot::pane_name::PaneRef> {
        Self::pane_refs_in(&observe::shared().fresh(observe::NOW))
    }

    pub(crate) fn pane_refs_in(obs: &observe::Observation) -> Vec<marspot::pane_name::PaneRef> {
        pane_control::names::pane_refs_in(obs)
    }

    /// Every pane as `(id, cwd, address)` — name and cell both, so a
    /// listing answers "how do I say this one again?" without the
    /// reader having to work out the numbering.
    pub(super) fn addressed_panes() -> Vec<(u64, String, String)> {
        let refs = Self::pane_refs();
        let named: std::collections::HashMap<u64, String> =
            marspot::pane_name::assign(&refs).into_iter().collect();
        let cells = pane_control::names::layout_cells();
        refs.into_iter()
            .map(|marspot::pane_name::PaneRef { sid, cwd, .. }| {
                let name = named.get(&sid).cloned().unwrap_or_default();
                let addr = match cells.get(&sid) {
                    Some((w, Some((x, y)))) => format!("{name}  w({w},{x},{y})"),
                    _ => name,
                };
                (sid, cwd, addr)
            })
            .collect()
    }

    /// The pane's screen as it is showing it.
    pub(super) fn read_pane_screen(sid: u64) -> Option<String> {
        marspot::pane_read::screen(sid, 0).ok()
    }

    /// Turn what a caller said into one pane.
    ///
    /// Three forms, because the useful address depends on what the
    /// caller knows: a **name** when it knows the project, an **id**
    /// when it was handed one, and a **cell** when it means "whatever
    /// is in that slot of that window".
    pub(super) fn resolve_target(&self, target: &str) -> Result<u64, String> {
        pane_control::names::resolve(target, &observe::shared().fresh(observe::NOW))
    }

    /// What the pane called `target` says.
    ///
    /// Read from what the session has already left behind -- its
    /// published frame, and its bytelog for history -- rather than asked
    /// of L3: no round trip, no state, and a pane that is
    /// mid-reclamation answers as readily as one nobody has touched.
    pub(super) fn run_cli_read(&self, target: &str, extra_lines: u32) -> Result<String, String> {
        let sid = self.resolve_target(target)?;
        read_text(sid, extra_lines).map_err(|e| format!("cannot read pane {sid}: {e}"))
    }

    /// Type `text` into the pane called `target`, then Enter.
    ///
    /// The whole of "session-to-session communication" so far, and
    /// deliberately so: it is the same delivery every future protocol
    /// needs, and everything hard about it is already here — naming a
    /// pane that has no name, not colliding with whatever that pane is
    /// already doing, and putting multi-line text in front of a program
    /// without it being executed a line at a time.
    pub(super) fn run_cli_send(&mut self, target: &str, text: &str) -> (bool, String) {
        let sid = match self.resolve_target(target) {
            Ok(sid) => sid,
            Err(e) => return (false, e),
        };
        if self.pty_ops.is_busy(sid) {
            // Refuse rather than queue: the caller is a person or a
            // program that wants to know its message went in *now*, and
            // a message that lands after a reclamation finishes has
            // landed somewhere else than the sender meant.
            return (
                false,
                format!(
                    "pane {sid} is busy ({})",
                    self.pty_ops.running(sid).unwrap_or("?")
                ),
            );
        }
        let op = plugins::pty_op::PtyOp::new("cli.send")
            .typed_for_program()
            // Nothing is frozen and no keys are locked: this is a
            // delivery, not a takeover.  The user can keep typing in
            // that pane while it happens.
            .lock_keys(false)
            .step(plugins::pty_op::Step::paste(text))
            // Enter separately, and *after* the paste has been handed
            // over: inside the pasted text it would be part of the
            // message, and bracketed paste is exactly the mechanism
            // that stops a newline from submitting.
            .step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
        match self.pty_ops.submit(sid, op) {
            Some(id) => (true, format!("queued on pane {sid} (id={})", id.0)),
            None => (false, format!("pane {sid} has too much queued")),
        }
    }
}

/// Pane `sid`'s screen under `extra_lines` of history, without the
/// status lines runs typed.
fn read_text(sid: u64, extra_lines: u32) -> std::io::Result<String> {
    let text = marspot::pane_read::screen(sid, extra_lines.min(u16::MAX as u32) as u16)?;
    Ok(pane_control::run_trailer::drop_status_lines(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::session_registry::{SessionEntry, write_session_entry};

    #[test]
    fn pane_control_read_leaves_the_status_line_out() {
        let sandbox = marspot_term::test_env::sandbox_state_dir();
        let sid = 4243u64;
        write_session_entry(&SessionEntry {
            id: sid,
            pid: 1,
            socket: sandbox.path().join("s.sock"),
            cols: 100,
            rows: 10,
            title: String::new(),
            cwd: "/tmp".into(),
            proto_version: marspot_term::shell_proto::PROTO_VERSION,
            created_at_unix: 0,
            shm_name: String::new(),
            shell_child_pid: 0,
            build: String::new(),
            stamps_regions: false,
            shadow_of: 0,
        })
        .unwrap();
        let dir = marspot_term::paths::sessions_dir().join(sid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let typed = marspot::shell_proto::run_trailer(false);
        let shown = format!("bash-3.2$ echo hi\r\nhi\r\nbash-3.2$ {typed}\r\nbash-3.2$ ");
        std::fs::write(dir.join("bytelog"), shown).unwrap();
        assert_eq!(read_text(sid, 0).unwrap().trim_end(), "bash-3.2$ echo hi\nhi\nbash-3.2$\nbash-3.2$");
    }
}

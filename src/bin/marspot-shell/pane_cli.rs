//! Naming and finding panes for the CLI, and reading from or typing into
//! one on its behalf.

use super::*;

impl ShellApp {
    /// Every pane in `obs` as `(session id, cwd, title)`.  The cwd is
    /// read live off the pane's foreground program (the shell when it is
    /// in front) rather than taken from the
    /// registry: the registry's copy is from spawn time and says the
    /// launch directory for every pane that has since cd'd somewhere.
    /// Only a live session's shell is read: a dead one's pid may name
    /// any process by now.
    pub(super) fn live_panes(obs: &observe::Observation) -> Vec<(u64, String, String)> {
        obs.sessions
            .iter()
            .map(|s| {
                let e = &s.entry;
                let cwd = s.shell.and_then(|id| marspot::pidtree::pane_cwd(id.pid));
                let cwd = cwd.map(|p| p.to_string_lossy().into_owned());
                (e.id, cwd.unwrap_or_else(|| e.cwd.clone()), e.title.clone())
            })
            .collect()
    }

    /// Every pane with what naming needs: its directory and its code.
    /// One place builds this, so the listing and `--send` cannot
    /// disagree about which pane a name means.
    fn pane_refs() -> Vec<marspot::pane_name::PaneRef> {
        Self::pane_refs_in(&observe::shared().fresh(observe::NOW))
    }

    /// `pane_refs` over a given observation.  May read the code file,
    /// so not for the main thread's hot paths.
    pub(crate) fn pane_refs_in(obs: &observe::Observation) -> Vec<marspot::pane_name::PaneRef> {
        Self::live_panes(obs)
            .into_iter()
            .map(|(sid, cwd, _)| marspot::pane_name::PaneRef::new(sid, cwd))
            .collect()
    }

    /// Every pane as `(id, cwd, address)` — name and cell both, so a
    /// listing answers "how do I say this one again?" without the
    /// reader having to work out the numbering.
    pub(super) fn addressed_panes() -> Vec<(u64, String, String)> {
        let refs = Self::pane_refs();
        let named: std::collections::HashMap<u64, String> =
            cli_socket::assign_names(&refs).into_iter().collect();
        let cells = Self::pane_cells();
        refs.into_iter()
            .map(|marspot::pane_name::PaneRef { sid, cwd, .. }| {
                let name = named.get(&sid).cloned().unwrap_or_default();
                let addr = match cells.get(&sid) {
                    Some((w, x, y)) => format!("{name}  w({w},{x},{y})"),
                    None => name,
                };
                (sid, cwd, addr)
            })
            .collect()
    }

    /// `sid → (window, x, y)`, 1-based, from the layout L2 persists.
    fn pane_cells() -> std::collections::HashMap<u64, (usize, usize, usize)> {
        let mut out = std::collections::HashMap::new();
        let Some(saved) = marspot::state::read() else {
            return out;
        };
        for (wi, win) in saved.windows.iter().enumerate() {
            let cols = win.grid_cols.max(1) as usize;
            for (i, p) in win.panes.iter().enumerate() {
                if p.sid != 0 {
                    out.insert(p.sid, (wi + 1, i % cols + 1, i / cols + 1));
                }
            }
        }
        out
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
        let panes = Self::pane_refs();
        match cli_socket::parse_target(target)? {
            cli_socket::Target::Id(sid) => {
                if panes.iter().any(|p| p.sid == sid) {
                    Ok(sid)
                } else {
                    Err(format!("no pane with session id {sid}"))
                }
            }
            cli_socket::Target::Name(n) => cli_socket::resolve_name(&n, &panes),
            cli_socket::Target::Cell { w, x, y } => Self::pane_at_cell(w, x, y),
        }
    }

    /// The pane occupying window `w`'s cell `(x, y)`, all 1-based.
    ///
    /// Read from the layout L2 persists on every spawn / close / focus
    /// change / layout apply, which is the only place the arrangement
    /// is known — L1 owns the windows, L2 decides what sits where.
    fn pane_at_cell(w: usize, x: usize, y: usize) -> Result<u64, String> {
        let saved = marspot::state::read()
            .ok_or_else(|| "no window layout on disk yet".to_string())?;
        let win = saved
            .windows
            .get(w - 1)
            .ok_or_else(|| format!("there is no window {w} (there are {})", saved.windows.len()))?;
        let (cols, rows) = (win.grid_cols as usize, win.grid_rows as usize);
        if x > cols || y > rows {
            return Err(format!("window {w} is {cols}×{rows}; ({x},{y}) is outside it"));
        }
        let idx = (y - 1) * cols + (x - 1);
        match win.panes.get(idx) {
            Some(p) if p.sid != 0 => Ok(p.sid),
            _ => Err(format!("window {w} cell ({x},{y}) is empty")),
        }
    }

    /// What the pane called `target` says.
    ///
    /// Read from what the session has already left behind -- its
    /// published frame, and its bytelog for history -- rather than asked
    /// of L3: no round trip, no state, and a pane that is
    /// mid-reclamation answers as readily as one nobody has touched.
    pub(super) fn run_cli_read(&self, target: &str, extra_lines: u32) -> Result<String, String> {
        let sid = self.resolve_target(target)?;
        marspot::pane_read::screen(sid, extra_lines.min(u16::MAX as u32) as u16)
            .map_err(|e| format!("cannot read pane {sid}: {e}"))
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

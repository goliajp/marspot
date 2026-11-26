//! What the open process monitor holds between frames.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use marspot::pidtree::{ProcId, ProcNode};

/// F3+1 — open process-tree panel state.  Holds per-pane trees +
/// a last-refresh timestamp so the main loop can decide when to
/// re-walk libproc (≥2 s gap = stale, refresh on next render).
/// `None` on `CoreApp.process_panel` means the panel is closed —
/// libproc is NOT walked at all in that state, so the panel has
/// zero idle cost when invisible.
pub(crate) struct ProcessPanelState {
    /// Per pane: (shelld_session_id, root shell pid, optional tree
    /// rooted at the shell child pid).  Tree is None when the pid
    /// can't be resolved (session has no entry.toml yet, or the
    /// shell child died before walk).  Indexed parallel to
    /// CoreApp.panes so the renderer can pick by pane order.  The
    /// processes closed panes left come last, as one more entry.
    pub(crate) panes: Vec<PanePidTree>,
    pub(crate) last_refresh: Instant,
    /// Each row of the process table, in the order drawn: the pid with
    /// the start time it had when the row was listed, so a press ends
    /// that process and never one that took its pid since.
    pub(crate) row_ids: Vec<ProcId>,
    /// F3+1.3 — kills awaiting SIGTERM→SIGKILL escalation, with when
    /// the SIGTERM went: past KILL_ESCALATION_GRACE a process still
    /// running under the same identity gets SIGKILL.
    pub(crate) pending_kills: Vec<(ProcId, Instant)>,
    /// F3+4 — which pane (row in the master column) is currently
    /// selected; the detail column shows that pane's tree.  Clamped
    /// each render so a closed pane doesn't strand the selection.
    pub(crate) selected_pane: usize,
    /// F3+4 — previous-sample CPU times per pid + sample wall clock,
    /// used to derive CPU% via delta.  Pids that don't reappear on
    /// the next refresh get dropped.  Cleared when the panel is
    /// closed so idle has no carrying state.
    pub(crate) prev_pid_stats: HashMap<i32, (u64, Instant)>,
    /// Each listed process's start time, from the walk its tree came from.
    pub(crate) starts: HashMap<i32, u64>,
    /// Where everything in the frame on screen is; clicks, the
    /// pointer and the wheel are all answered from it.  `None` until
    /// the first frame is built.
    pub(crate) layout: Option<marspot::ui::components::process_panel::Layout>,
    pub(crate) surface: std::rc::Rc<std::cell::RefCell<marspot::ui::components::process_view::ProcessSurface>>,
    /// How far the process list is pushed up, in physical pixels.
    pub(crate) scroll_y: marspot::ui::Units,
}

impl ProcessPanelState {
    pub(crate) fn new() -> ProcessPanelState {
        ProcessPanelState {
            panes: Vec::new(),
            last_refresh: Instant::now() - Duration::from_secs(10),
            row_ids: Vec::new(),
            pending_kills: Vec::new(),
            selected_pane: 0,
            prev_pid_stats: HashMap::new(),
            starts: HashMap::new(),
            layout: None,
            surface: Default::default(),
            scroll_y: Default::default(),
        }
    }
}

pub(crate) struct PanePidTree {
    pub(crate) shelld_session_id: u64,
    pub(crate) shell_child_pid: Option<i32>,
    pub(crate) tree: Option<ProcNode>,
    /// The entry of the processes closed panes left, each tree under
    /// the line its row is labelled with; None for a pane.
    pub(crate) orphans: Option<Vec<(String, ProcNode)>>,
    /// F3+4 — aggregate stats over the tree, populated by
    /// `refresh_process_panel` from the latest pidtree + proc_stat
    /// sample.  Master column in the renderer reads these.
    pub(crate) name: String,
    pub(crate) n_pids: u32,
    pub(crate) cpu_pct: f32,
    pub(crate) rss_kb: u64,
    pub(crate) busy: String,
}

/// F3+1.3 — how long after a SIGTERM before we escalate to SIGKILL.
/// Chosen for "user clicks [×] and expects the row to vanish soon":
/// 2 s is long enough that a graceful shutdown (claude flushing logs,
/// node closing the socket) can complete, short enough that the user
/// doesn't think the click was ignored.
pub(crate) const KILL_ESCALATION_GRACE: Duration = Duration::from_secs(2);

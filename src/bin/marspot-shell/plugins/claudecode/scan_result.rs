//! What one scan hands the tick.

use std::collections::HashMap;
use std::time::SystemTime;

use super::*;

pub(super) struct ScanResult {
    /// `shelld_session_id → badge string ("P<n> <uuid>")`.  Replaces
    /// `last_mapping` on the main side every time it arrives.
    pub(super) new_mapping: HashMap<u64, String>,
    /// `shelld_session_id → what cc is doing there`.  Reported to the
    /// shell, which folds it into the pane's state machine.
    pub(super) new_activity: HashMap<u64, CcActivity>,
    /// `shelld_session_id → the model it is working in`, short token
    /// (`fable-5`, `opus-5-5`).
    ///
    /// The badge already carries this for display. The quota chooser
    /// needs it as a fact: some projects can only run on Fable, and
    /// Fable has a cap of its own on top of the account's windows, so
    /// moving such a pane to an account with a fresh week but a spent
    /// Fable moves it nowhere.
    pub(super) new_models: HashMap<u64, String>,
    /// `shelld_session_id → (claude subtree CPU ns, sampled at)`.
    /// Taken in the same pass as the bindings so the idle policy
    /// compares like with like.
    pub(super) new_cpu: HashMap<u64, (u64, SystemTime)>,
    /// `shelld_session_id → (work of its own running, waiting on its
    /// own timer)`.  Both are vetoes on reclamation that the clock and
    /// the CPU sample cannot see.
    pub(super) new_vetoes: HashMap<u64, (bool, bool)>,
    /// When this scan's facts were gathered.  A dormant record newer
    /// than this must not be judged by it — see `DormantRecord`.
    pub(super) scanned_at: SystemTime,
    /// Every live session the scan looked at, bound or not.  The ones
    /// missing from `new_activity` get reported as `Absent` — "claude
    /// is not in this pane" is an answer the machine needs, and it
    /// cannot be inferred from a missing key (that could equally mean
    /// the scan never ran).
    pub(super) sessions_seen: Vec<u64>,
    /// `shelld_session_id → BindMeta`.  Drives `on_pane_badge_click`'s
    /// profile-cycle dispatch on the main side.
    pub(super) new_meta: HashMap<u64, BindMeta>,
    /// Log lines the worker wanted to emit but can't (host is main-
    /// thread-only).  `tick` replays them through `host.log`.  Stays
    /// near-empty in steady state — only transitions add lines.
    pub(super) log_lines: Vec<(LogLevel, &'static str, String)>,
    /// Every `~/.claude-profile-N` with its account address, read here
    /// so the main thread's quota sweep does not touch the disk.
    pub(super) profiles: Vec<(u8, Option<String>)>,
    /// The usage feed as this pass read it; None when there is none.
    pub(super) usage: Option<marspot::cc_usage::CcUsage>,
    /// Each bound pane's model and effort as its status-line hook last
    /// pushed them (`pushed_choice`), by pane.
    pub(super) choices: HashMap<u64, CliChoice>,
    /// Per pane: the quota refusal its newest background-agent report
    /// ended on, when that agent was refused under this claude.
    pub(super) agent_refused: HashMap<u64, quota::Refusal>,
}

/// The files the quota sweep judges by besides the bindings: the usage
/// feed and each bound pane's pushed choice.  Read on the scan thread,
/// so the tick that acts on them waits on no disk (2026-11-10: the
/// sweep, reading these itself, held the shell's main thread for up to
/// 1.8 s while the machine was busy).
pub(super) fn quota_inputs(
    meta: &HashMap<u64, BindMeta>,
) -> (Option<marspot::cc_usage::CcUsage>, HashMap<u64, CliChoice>) {
    let choices = meta.iter().map(|(sid, m)| (*sid, pushed_choice(&m.uuid))).collect();
    (marspot::cc_usage::read(), choices)
}

//! What the scan knows about a pane's session, and what has been spent moving it.

use super::*;

/// Per-session state cached so we only re-log on real change.
#[derive(Clone, Debug)]
pub(super) struct SessionInfo {
    pub(super) session_id: String,
    pub(super) project_dir: String, // encoded form, e.g. -Users-ada-workspace-...
    pub(super) jsonl_path: PathBuf,
    pub(super) last_mtime: SystemTime,
    pub(super) last_size: u64,
    /// Holds at least one user or assistant record, so `--resume` can
    /// open it. See `transcript::has_conversation`.
    pub(super) has_conversation: bool,
}

/// What has been spent on one stranded pane.
#[derive(Clone, Copy)]
pub(super) struct Stranding {
    /// Moves made while this pane has been continuously refused.
    pub(super) moves: u8,
    /// Where the last one was aimed, so a move that has not taken
    /// effect yet is not made twice.
    pub(super) last_target: Option<u8>,
    /// When the pane was first seen stuck on an account that is out.
    pub(super) since: Instant,
    /// The reason for holding last said.  A hold is a steady state,
    /// and the sweep runs every two seconds: said every pass, one held
    /// pane wrote 170 identical lines (2026-09-29, sid 430).  Said only
    /// once, a pane held from 01:26 to 02:59 left one line, and nothing
    /// showed what kept it there (2026-10-08).  So: again whenever it
    /// changes.
    pub(super) hold_said: Option<&'static str>,
}

impl Default for Stranding {
    fn default() -> Self {
        Self { moves: 0, last_target: None, since: Instant::now(), hold_said: None }
    }
}

#[derive(Clone, Debug)]
pub(super) struct BindMeta {
    /// Which binding pass named the session: `record`,
    /// `record_overridden`, `argv`, `argv_successor`,
    /// `newest_transcript`, `statusline_hook`, or `unbound`.
    pub(super) bound_by: &'static str,
    /// 0 = default `.claude`, N = `.claude-profile-N`, 255 = unknown.
    pub(super) profile_num: u8,
    /// `CLAUDE_CONFIG_DIR` as read off the live process — the ground
    /// truth of which profile this session belongs to.
    ///
    /// The resume line uses THIS rather than reconstructing
    /// `claude<N>`: those are interactive shell aliases
    /// (`alias claude1='CLAUDE_CONFIG_DIR=~/.claude-profile-1 claude'`),
    /// so reproducing them depends on the user's rc file still
    /// defining them, in that shell, at that moment.  Setting the
    /// variable we observed is the alias's own expansion, made
    /// explicit and independent of all of that.
    pub(super) config_dir: Option<String>,
    pub(super) uuid: String,
    /// The claude this binding was read off; what any action on the
    /// pane re-verifies before it signals or types.
    pub(super) claude: pidtree::ProcId,
    /// Basename of the claude process's cwd — used as the pane title
    /// so user sees "marspot" instead of "session-3" once cc binds.
    /// Empty when basename couldn't be resolved (e.g. process exited
    /// between scan and tick).
    pub(super) project_basename: String,
    /// The whole of that cwd, which is where the session lives.
    ///
    /// `claude --resume` resolves a session id against the project of
    /// the directory it is run from, and binds what it reopens to that
    /// directory -- rules, hooks and the pane's title follow. The
    /// pane's shell is not reliably standing there: a profile switch
    /// that types the resume wherever the shell happens to be moved a
    /// torajs session into devops, and the only visible sign was the
    /// title changing.
    pub(super) project_dir: Option<String>,
    /// Unix seconds this session was last refused by the API for want
    /// of quota, from its own transcript.
    ///
    /// The feed is collected behind what a session spends, and can be
    /// behind by a lot -- it carries a row's model caps forward when it
    /// cannot re-measure them, and one stood at 0.62 for 94 minutes
    /// while this message was written sixteen times. So the two are
    /// compared by when each was true, and the later one is believed.
    pub(super) refused_at: Option<quota::Refusal>,
    /// When this session's transcript was last written.
    ///
    /// The honest answer to "how long has this session been idle".
    /// The pane's own quiet clock is not: claude prints `Checking for
    /// updates` in the corner every thirty minutes, which makes the
    /// terminal busy for half a minute and resets that clock — on this
    /// machine it fired 23 times in one pane's log, so a session idle
    /// for eleven hours never once accumulated thirty quiet minutes.
    /// Chrome does not touch the transcript.
    pub(super) transcript_at: SystemTime,
    /// Whether claude has written this session's transcript yet.
    ///
    /// False is not a gap — it is the defining fact about a session
    /// that has not taken a turn: there is nothing to `--resume`, so
    /// the profile cycle starts claude plain instead.  See
    /// [`profile_cycle_op`].
    pub(super) has_transcript: bool,
}

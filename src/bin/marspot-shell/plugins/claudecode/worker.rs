//! The scan that feeds every tick, and the thread it runs on.
//!
//! Binding panes to claude sessions means reading transcripts off the
//! disk — a directory per project, a JSONL per session, tails of each.
//! None of that may happen on the thread that draws the window, so it
//! happens here: one worker, one request per tick, one owned result
//! sent back.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

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
}

/// Everything the worker owns.  No shared mutable state with the
/// plugin; the worker reads disk + procs and sends ScanResult back.
pub(super) struct WorkerCtx {
    pub(super) projects_root: PathBuf,
    pub(super) shelld: Arc<ShelldClient>,
    /// Last `(switch value, time)` the status-line hook was
    /// reconciled against, so the usual scan does not re-read Claude
    /// Code's settings every two seconds.
    pub(super) statusline_state: Option<(bool, Instant)>,
    /// Same role as the old `ClaudecodePlugin::seen` field, but the
    /// worker owns it now and the plugin never touches it.
    pub(super) seen: HashMap<PathBuf, SessionInfo>,
    /// Per-jsonl: the claude pid last seen owning it, and the byte
    /// offset from which its model may be read.  See
    /// `model_cutoff_for`.
    pub(super) model_cutoff: HashMap<PathBuf, (i32, u64)>,
    /// When this pane's screen was last searched for a startup banner.
    /// Bounds `model_from_banner` to one bytelog replay per pane per
    /// window, so a pane that will never show one costs nothing.
    pub(super) banner_tried: HashMap<u64, Instant>,
    /// Per-jsonl: the last model actually read out of it.
    ///
    /// The fence answers "what may I read right now", which is not
    /// the same question as "what is this session running".  See
    /// `model_for`.
    pub(super) last_model: HashMap<PathBuf, ModelBadge>,
    /// Per pane: the last model half its badge carried.
    ///
    /// Keyed by pane, not by file, because the pane that needs it has
    /// no file yet.  Its sources answer None on some ticks and Some on
    /// others — the banner read is rate-limited, the hook has not
    /// necessarily run — and a badge that follows that blinks between
    /// `P7` and `P7@opus-5-5` twice a second (2026-09-23 report).
    /// What was true a moment ago is the better answer.
    pub(super) last_model_by_pane: HashMap<u64, ModelBadge>,
}

/// How many of a project's session files stay in `seen`.
///
/// One was not enough: with a single candidate per project, two panes
/// in one project can never both be badged — the first takes it and the
/// second has nothing to fall back to even when its own session is live.
/// Four covers the realistic "a couple of panes in one repo" case with
/// headroom, and bounds the map at panes × 4 entries.
pub(super) const SESSIONS_KEPT_PER_PROJECT: usize = 4;

impl WorkerCtx {
    /// Refresh `seen` for exactly the projects named in `wanted` —
    /// the ones that have a live pane.
    ///
    /// Scoping matters twice over.  It used to walk every directory
    /// under `~/.claude/projects` (~50 here, of which ~10 have panes),
    /// paying a `parse_session_id` + `tail_last_message_type` per newly
    /// changed file for projects nothing would ever ask about; and
    /// `seen` was insert-only, so a long-lived shell accumulated an
    /// entry per session file it had ever noticed.  Scoped + pruned,
    /// the map is bounded by `wanted.len() × SESSIONS_KEPT_PER_PROJECT`
    /// and the walk touches fewer directories than before despite
    /// keeping four files each instead of one.
    pub(super) fn refresh_seen<'a>(
        &mut self,
        wanted: impl Iterator<Item = (&'a std::path::Path, &'a str)>,
        log_lines: &mut Vec<(LogLevel, &'static str, String)>,
    ) {
        let mut newly_seen = 0usize;
        let mut updates = 0usize;
        let mut alive: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut done: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (root, project_dir) in wanted {
            let project_path = root.join(project_dir);
            // Keyed by the full path, not the project name: the same
            // project open under two profiles is two directories, and
            // de-duping on the name alone would walk only whichever
            // pane happened to come first.
            if !done.insert(project_path.clone()) {
                continue; // two panes, one project — walk it once
            }
            let Ok(dir) = fs::read_dir(&project_path) else { continue };
            let mut cands: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
            for f in dir.flatten() {
                let p = f.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                cands.push((
                    p,
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    meta.len(),
                ));
            }
            cands.sort_by_key(|c| std::cmp::Reverse(c.1));
            cands.truncate(SESSIONS_KEPT_PER_PROJECT);
            for (jsonl_path, mtime, size) in cands {
                alive.insert(jsonl_path.clone());
                if let Some(prev) = self.seen.get(&jsonl_path)
                    && prev.last_mtime == mtime && prev.last_size == size {
                        continue;
                    }
                let Some(session_id) = parse_session_id(&jsonl_path) else { continue };
                let last_message_kind = tail_last_message_type(&jsonl_path);
                let is_new = !self.seen.contains_key(&jsonl_path);
                self.seen.insert(
                    jsonl_path.clone(),
                    SessionInfo {
                        session_id: session_id.clone(),
                        project_dir: project_dir.to_string(),
                        jsonl_path: jsonl_path.clone(),
                        last_mtime: mtime,
                        last_size: size,
                        last_message_kind: last_message_kind.clone(),
                    },
                );
                if is_new {
                    newly_seen += 1;
                    log_lines.push((
                        LogLevel::Info,
                        "session",
                        format!(
                            "session detected sid={} project={} kind={} size={}",
                            session_id,
                            project_dir,
                            last_message_kind.as_deref().unwrap_or("?"),
                            size
                        ),
                    ));
                } else {
                    updates += 1;
                    log_lines.push((
                        LogLevel::Debug,
                        "session.update",
                        format!(
                            "sid={} kind={} size={}",
                            session_id,
                            last_message_kind.as_deref().unwrap_or("?"),
                            size
                        ),
                    ));
                }
            }
        }
        // Bounded growth: anything that dropped out of a project's top
        // N (or whose project lost its last pane) leaves the map, and
        // the per-file model fence leaves with it.
        self.seen.retain(|p, _| alive.contains(p));
        self.model_cutoff.retain(|p, _| alive.contains(p));
        self.last_model.retain(|p, _| alive.contains(p));
        if newly_seen > 0 || updates > 0 {
            log_lines.push((
                LogLevel::Debug,
                "tick.summary",
                format!("new={} updated={}", newly_seen, updates),
            ));
        }
    }

    /// Byte offset in `path` from which an assistant record may be
    /// trusted to describe the model `claude_pid` is actually using.
    ///
    /// A profile switch kills claude and re-runs it as
    /// `claude<N> --resume <uuid>` — the **same** session, so the same
    /// jsonl simply keeps growing.  Nothing is written at resume that
    /// names the new model (checked: the only startup-ish records are
    /// `mode`, `last-prompt` and `system/turn_duration`, none of which
    /// carry one), so the newest assistant record in the file is still
    /// the one the *previous* profile served — with the previous
    /// profile's model.  Reading it back gives a badge that confidently
    /// states the wrong model until the user sends another message, and
    /// profiles really do differ here.
    ///
    /// A changed pid is the signal that the file's existing contents
    /// belong to a process that is gone.  Everything before that point
    /// is fenced off; until the new process answers a turn, the badge
    /// renders without `@model` — which is the honest state, and one
    /// the badge already knows how to draw.
    pub(super) fn model_cutoff_for(&mut self, path: &std::path::Path, claude_pid: i32) -> u64 {
        match self.model_cutoff.get(path) {
            Some(&(pid, cutoff)) if pid == claude_pid => cutoff,
            _ => {
                // Either the first sighting or a replaced process.  On
                // first sighting the records are this process's own, so
                // nothing is fenced (cutoff 0); on replacement, fence
                // everything written so far.
                let cutoff = if self.model_cutoff.contains_key(path) {
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                self.model_cutoff.insert(path.to_path_buf(), (claude_pid, cutoff));
                cutoff
            }
        }
    }

    /// Keep Claude Code's registration of the hook in line with the
    /// switch.
    ///
    /// Re-checked on a change of the switch, and otherwise once a
    /// minute — slowly, because what it catches between switch flips
    /// is a binary that moved under an installed hook, or a
    /// settings.json somebody edited by hand.  Reading the switch is a
    /// map lookup; the minute is what keeps the two small file reads
    /// off the two-second scan.
    pub(super) fn reconcile_statusline(&mut self) -> Vec<String> {
        const RECHECK: Duration = Duration::from_secs(60);
        let want = marspot::settings::get().cc_statusline_hook;
        let now = Instant::now();
        if let Some((was, at)) = self.statusline_state
            && was == want && now.duration_since(at) < RECHECK {
                return Vec::new();
            }
        self.statusline_state = Some((want, now));
        reconcile_statusline_hook(want)
    }

    /// The model to show for this session, best source first.
    ///
    /// 1. **What claude reports.**  Its status-line hook carries the
    ///    model claude currently believes it is on, pushed on state
    ///    change.  Needs no fence and is never turn-lagged — but it
    ///    only exists where the hook is installed, which is nowhere by
    ///    default, so everything below has to stand on its own.
    /// 2. **The transcript, behind the fence.**  Authoritative when it
    ///    speaks, and it speaks only at turn boundaries and at
    ///    `/model`.  A resumed process writes nothing that names a
    ///    model until it finishes a turn (the startup records are
    ///    `mode` and `permission-mode`, neither carries one) and the
    ///    fence sits at end-of-file, so right after a profile switch
    ///    there is nothing readable here at all.
    /// 3. **The pane's own screen.**  Which is where the answer has
    ///    been all along in exactly that case: a resumed claude
    ///    reprints its startup banner, and the banner names the model.
    ///    This used to sit behind `last_model` at the call site and so
    ///    was never reached — the badge kept showing the model of the
    ///    profile that had just been cycled away from, for as long as
    ///    the pane stayed parked.  It is a replay, so it is rate
    ///    limited by `model_from_banner` itself.
    /// 4. **The last model actually seen.**  Rendering no model at all
    ///    is a lie by omission: the session has one, we simply have
    ///    not watched it say so.  Last resort, and only now.
    pub(super) fn model_for(
        &mut self,
        path: &std::path::Path,
        claude_pid: i32,
        sid: u64,
    ) -> Option<ModelBadge> {
        let pushed = pushed_model(path);
        let cutoff = self.model_cutoff_for(path, claude_pid);
        if let Some((mut m, said)) = pushed {
            if said == EffortSaid::No {
                // Fill only the half the record could not carry, and
                // only from a record naming the same model — an
                // effort read off a turn served by a different model
                // would be a number about something else.
                if let Some(t) = tail_model_short(path, cutoff)
                    && t.model == m.model {
                        m.effort = t.effort;
                    }
            }
            self.last_model.insert(path.to_path_buf(), m.clone());
            return Some(m);
        }
        if let Some(m) = tail_model_short(path, cutoff) {
            self.last_model.insert(path.to_path_buf(), m.clone());
            return Some(m);
        }
        if let Some(m) = self.model_from_banner(sid) {
            self.last_model.insert(path.to_path_buf(), m.clone());
            return Some(m);
        }
        self.last_model.get(path).cloned()
    }

    /// The badge's model half for one pane.
    ///
    /// `model_for` answers it from a session FILE; this is the rest of
    /// the question, for the window where claude is running and that
    /// file does not exist yet — which is exactly when the user is
    /// looking at a pane they just opened.  In that window the hook's
    /// own record is the only source that carries the effort as well
    /// (the banner line prints the model and, unless it is the
    /// default, nothing else), and it lands a couple of seconds after
    /// claude starts.
    pub(super) fn model_for_pane(
        &mut self,
        path: Option<&std::path::Path>,
        uuid: &str,
        hook_said: Option<ModelBadge>,
        claude_pid: i32,
        sid: u64,
    ) -> Option<ModelBadge> {
        let found = match path {
            Some(p) => self.model_for(p, claude_pid, sid),
            // Whoever already found the hook's record for this pane
            // first, then the record for the session it is bound to —
            // one file read, keyed by the uuid, where the by-pid form
            // walks the directory.  The banner last: it is on screen
            // only until claude scrolls it off, and it has no effort.
            None => hook_said
                .or_else(|| pushed_by_uuid(uuid))
                .or_else(|| self.model_from_banner(sid)),
        };
        match found {
            Some(m) => {
                self.last_model_by_pane.insert(sid, m.clone());
                Some(m)
            }
            None => self.last_model_by_pane.get(&sid).cloned(),
        }
    }

    /// The model a **freshly started** pane is on, read off its own
    /// screen.
    ///
    /// A session names its model in the transcript only from the first
    /// assistant turn onward — records 1..10 are `mode`,
    /// `permission-mode`, attachments, none of which carry it
    /// (checked on a live file 2026-08-11).  So between opening a pane
    /// and its first answer the transcript genuinely cannot say, and
    /// the badge showed a bare `P1` for as long as the user took to
    /// type — which reads as "marspot has not noticed this pane".
    ///
    /// But claude prints it, in the banner, in the first frame:
    ///
    /// ```text
    /// Claude Code v2.1.227
    /// Opus 5 (1M context) with high effort · Claude Max
    /// ```
    ///
    /// The pane's own screen is therefore the earliest source there
    /// is, and `pane_read` already knows how to replay a bytelog into
    /// a grid.  Read only while the model is unknown — the transcript
    /// takes over the moment it has one, and a fresh session's bytelog
    /// is small, so the replay this costs is a young pane's alone.
    pub(super) fn model_from_banner(&mut self, sid: u64) -> Option<ModelBadge> {
        // A pane that never shows a banner (not claude at all, screen
        // already scrolled past it) must not buy a replay every scan,
        // so attempts are rate-limited.  But the interval keys off the
        // bytelog's size rather than being one number, because the two
        // cases the limit serves are opposites: the pane that has just
        // opened is exactly the one we most want to read, and its log
        // is a few KB, so replaying it costs almost nothing.  A flat
        // 30 s meant a first attempt landing a moment before claude
        // painted its banner left the badge model-less for the next
        // half minute — the whole window in which the user is looking
        // at a freshly opened pane.
        const BANNER_RETRY: Duration = Duration::from_secs(30);
        const YOUNG_RETRY: Duration = Duration::from_secs(3);
        /// A log this small is a pane that has barely started; the
        /// replay is bounded by it, so frequent retries are bounded
        /// too.
        const YOUNG_BYTELOG: u64 = 256 * 1024;
        let bytelog = marspot_term::paths::sessions_dir()
            .join(sid.to_string())
            .join("bytelog");
        // One stat on the file we were going to read anyway.
        let young = std::fs::metadata(&bytelog)
            .map(|m| m.len() <= YOUNG_BYTELOG)
            .unwrap_or(false);
        let retry = if young { YOUNG_RETRY } else { BANNER_RETRY };
        let now = Instant::now();
        if let Some(&t) = self.banner_tried.get(&sid)
            && now.duration_since(t) < retry {
                return None;
            }
        self.banner_tried.insert(sid, now);
        let entry = marspot_term::session_registry::list_session_entries()
            .into_iter()
            .find(|e| e.id == sid)?;
        let screen = marspot::pane_read::screen_text(&bytelog, entry.cols, entry.rows, 0).ok()?;
        parse_banner_model(&screen)
    }
}

/// Worker thread entry.  Lives until the request channel is dropped
/// (which `stop()` triggers by clearing `scan_req_tx`).
pub(super) fn worker_main(
    mut ctx: WorkerCtx,
    req_rx: Receiver<()>,
    res_tx: Sender<ScanResult>,
) {
    while req_rx.recv().is_ok() {
        // Deliberately outside `scan_once`: this one reaches out of
        // marspot and edits Claude Code's settings, and `scan_once`
        // is called directly by tests that have no business doing
        // that to the machine they run on.  The tick owns it.
        for line in ctx.reconcile_statusline() {
            marspot::lx_info!("plugin.claudecode.statusline_hook", &line);
        }
        let result = ctx.scan_once();
        if res_tx.send(result).is_err() {
            // Main side hung up; nothing left to do.
            break;
        }
    }
}

impl WorkerCtx {
    /// One full scan pass: walk `~/.claude/projects/*/*.jsonl`,
    /// update `self.seen`, list shelld sessions, BFS each for a
    /// `claude` descendant, and compute the new badge mapping.
    /// Slow (disk + sysctl) but runs off the L1 main loop so its
    /// runtime is invisible to the plugin host's 100ms tick budget.
    pub(super) fn scan_once(&mut self) -> ScanResult {
        let mut log_lines: Vec<(LogLevel, &'static str, String)> = Vec::new();

        // -- per-session mapping: BFS each shelld session ------------
        let mut new_mapping: HashMap<u64, String> = HashMap::new();
        let mut new_meta: HashMap<u64, BindMeta> = HashMap::new();
        let mut new_activity: HashMap<u64, CcActivity> = HashMap::new();
        let mut sessions_seen: Vec<u64> = Vec::new();
        let scanned_at = SystemTime::now();
        let mut new_cpu: HashMap<u64, (u64, SystemTime)> = HashMap::new();
        let mut new_vetoes: HashMap<u64, (bool, bool)> = HashMap::new();
        let sessions = match self.shelld.list_sessions() {
            Ok(v) => v,
            Err(e) => {
                log_lines.push((
                    LogLevel::Info,
                    "tick.shelld_list_failed",
                    format!("{e}"),
                ));
                return ScanResult { new_mapping, new_meta, new_activity, new_models: HashMap::new(), new_cpu, new_vetoes, scanned_at, sessions_seen, log_lines };
            }
        };
        let procs = pidtree::list_all_procs();
        // Pass 1 — the per-pane facts, gathered before anything is
        // bound.  Binding needs to be a decision over the whole set:
        // one session uuid belongs to exactly one pane, so a pane that
        // can *prove* its uuid (argv) has to be served before a pane
        // that is only guessing from mtimes.
        struct PaneFacts {
            shelld_sid: u64,
            claude_pid: i32,
            claude_start: SystemTime,
            cwd: PathBuf,
            encoded: String,
            argv_uuid: Option<String>,
            /// `CLAUDE_CONFIG_DIR` as this pane's claude was started
            /// with — read **once** here and used for everything that
            /// depends on it.
            config_dir: Option<String>,
            /// Where *this* pane's transcripts live.
            ///
            /// `~/.claude/projects` is only right for a pane running
            /// the default profile.  A pane started with
            /// `CLAUDE_CONFIG_DIR=~/.claude-profile-3` writes under
            /// that directory instead, so scanning the default one
            /// found no transcript, and the badge lost its `@model`
            /// half for every non-default profile (2026-08-10 report:
            /// `torajs` on P3 — the tag was right because it reads the
            /// same env var, the model was missing because this did
            /// not).
            projects_root: PathBuf,
        }
        let mut facts: Vec<PaneFacts> = Vec::new();
        for s in &sessions {
            if !s.alive {
                continue;
            }
            sessions_seen.push(s.session_id);
            let descendants = pidtree::descendants_of(s.child_pid, &procs);
            let Some(claude) =
                descendants.iter().find(|d| looks_like_claudecode(d))
            else {
                continue;
            };
            let Some(cwd) = pidtree::proc_cwd(claude.pid) else {
                continue;
            };
            let config_dir = pidtree::proc_env_value(claude.pid, "CLAUDE_CONFIG_DIR");
            let projects_root = projects_root_from(config_dir.as_deref())
                .unwrap_or_else(|| self.projects_root.clone());
            facts.push(PaneFacts {
                shelld_sid: s.session_id,
                claude_pid: claude.pid,
                claude_start: SystemTime::UNIX_EPOCH
                    + Duration::from_secs(claude.start_unix),
                encoded: encode_project_dir(&cwd),
                cwd,
                argv_uuid: argv_session_uuid(claude.pid, &descendants),
                config_dir,
                projects_root,
            });
        }
        // Stable order so an ambiguous project resolves the same way on
        // every tick — badges that swap panes every 2 s would be worse
        // than a badge that is merely a guess.
        facts.sort_by_key(|f| f.shelld_sid);

        // -- jsonl pass, scoped to the projects that have panes -------
        self.refresh_seen(
            facts.iter().map(|f| (f.projects_root.as_path(), f.encoded.as_str())),
            &mut log_lines,
        );

        // Pass 2 — assign, proof first, guesses after, no uuid twice.
        let mut claimed: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut bound: Vec<(usize, String, Option<PathBuf>)> = Vec::new();

        // Proof first means the CLI's own word first.  Every live claude
        // writes `<config-dir>/sessions/<pid>.json` with the session it
        // is on *now* — it follows `/clear` and `/resume`, which argv
        // does not (a background job started as `--session-id 85ece795`
        // was recorded on `fb55e303` a day later).  Keyed by the pid the
        // pane is running, so there is nothing to infer.
        //
        // And a session a background job holds is never a pane's.  The
        // guesses below pick "the transcript written most recently in
        // this project since claude started", and a background job
        // (scheduled task, detached conversation) writes into the same
        // project directory — usually more often than the pane does.
        // That is how panes 390 and 391 came to be badged with, and
        // cycled against, sessions two background jobs owned: every
        // badge click was refused as "held by a background session"
        // (2026-09-17).  Claiming those uuids up front keeps the guesses
        // off them.
        let mut record_dirs: std::collections::BTreeSet<PathBuf> = Default::default();
        for f in &facts {
            record_dirs.insert(config_dir_or_default(f.config_dir.as_deref()));
        }
        for dir in &record_dirs {
            for uuid in background_held_sessions(dir) {
                claimed.insert(uuid);
            }
        }
        for (i, f) in facts.iter().enumerate() {
            let dir = config_dir_or_default(f.config_dir.as_deref());
            let Some(rec) = recorded_session(&dir, f.claude_pid) else { continue };
            if rec.kind != "interactive" || !claimed.insert(rec.session_id.clone()) {
                continue;
            }
            let path = self.session_by_uuid(&rec.session_id).map(|s| s.jsonl_path.clone());
            bound.push((i, rec.session_id, path));
        }
        // Models that came from the hook rather than from a transcript
        // — see pass 3.
        let mut pushed_model_by_pane: HashMap<usize, ModelBadge> = HashMap::new();
        for (i, f) in facts.iter().enumerate() {
            if bound.iter().any(|(j, _, _)| *j == i) {
                continue;
            }
            let Some(argv_uuid) = &f.argv_uuid else { continue };
            // argv says where this process *started*.  Inside the
            // session the user can move on — `/clear` opens a new
            // session, `/resume` picks another — and argv stays frozen
            // at whatever it was launched with.  So argv is a starting
            // position, and the session actually in front of the user
            // is the one being written.
            let uuid = self
                .successor_session(&f.encoded, &claimed, f.claude_start, argv_uuid)
                .unwrap_or_else(|| argv_uuid.clone());
            if claimed.insert(uuid.clone()) {
                let path = self.session_by_uuid(&uuid).map(|s| s.jsonl_path.clone());
                bound.push((i, uuid, path));
            }
        }
        for (i, f) in facts.iter().enumerate() {
            if bound.iter().any(|(j, _, _)| *j == i) {
                continue;
            }
            if let Some((uuid, path)) =
                self.session_for_project(&f.encoded, &claimed, f.claude_start)
            {
                claimed.insert(uuid.clone());
                bound.push((i, uuid, Some(path)));
            }
        }

        // Pass 3 — the sessions with no file yet, named by the hook.
        //
        // Passes 1 and 2 both end at a transcript, and claude writes
        // one on the first turn; a session sitting at its prompt has
        // none, so neither pass can name it.  Its status-line hook has
        // already run, though, and said which session it is — that is
        // the only channel that speaks before the first turn.
        for (i, f) in facts.iter().enumerate() {
            if bound.iter().any(|(j, _, _)| *j == i) {
                continue;
            }
            let Some((uuid, transcript, badge)) = pushed_session_for_pid(f.claude_pid)
            else {
                continue;
            };
            if !claimed.insert(uuid.clone()) {
                continue;
            }
            // The path only rides along once it exists: everything
            // downstream of it (idle clock, model tail, activity)
            // reads the file, and a path to a file that is not there
            // is not a better answer than none.
            let path = transcript.is_file().then_some(transcript);
            pushed_model_by_pane.insert(i, badge);
            bound.push((i, uuid, path));
        }

        // Every pane running claude gets an entry, bound or not.
        //
        // Binding needs a session file, and claude writes that file on
        // the first turn — so between `claude` starting and the user's
        // first prompt (minutes, in practice) a pane used to have no
        // badge at all, which reads as "marspot didn't notice".  The
        // profile is readable from the process the whole time, so the
        // honest badge in that window is `P1`: the account is known,
        // the model is not, and `@model` fills in on the tick after
        // the session file appears.
        let bound: HashMap<usize, (String, Option<PathBuf>)> = bound
            .into_iter()
            .map(|(i, uuid, path)| (i, (uuid, path)))
            .collect();
        for (i, f) in facts.iter().enumerate() {
            let (sid_uuid, jsonl_path) = match bound.get(&i) {
                Some((uuid, path)) => (uuid.clone(), path.clone()),
                None => (String::new(), None),
            };
            let (tag, profile_num) = match profile_tag_from(f.config_dir.as_deref()) {
                Some(t) => {
                    let n = t
                        .strip_prefix('P')
                        .and_then(|d| d.parse::<u8>().ok())
                        .unwrap_or(u8::MAX);
                    (Some(t), n)
                }
                None => (None, u8::MAX),
            };
            // Active model, tailed from the session jsonl: the
            // newest of (assistant record's authoritative
            // `"model"` field, `/model` local_command output) —
            // the latter makes an interactive switch show up on
            // the very next tick instead of after the next
            // assistant turn — falling back to the last one seen
            // when the fence has nothing readable behind it (see
            // `model_for`).  A session named by argv but not yet
            // scanned has no path — badge without the model half.
            let model = self.model_for_pane(
                jsonl_path.as_deref(),
                &sid_uuid,
                pushed_model_by_pane.get(&i).cloned(),
                f.claude_pid,
                f.shelld_sid,
            );
            // The session uuid used to ride along here.  It is 36
            // characters of hex that no one can act on — it names the
            // session for a *machine*, and every machine that needs it
            // (the log, `dormant.tsv`, the resume line) has it already.
            // On screen it crowded out the pane's own title and told
            // the reader nothing.
            let badge = match (tag, model) {
                (Some(t), Some(m)) => format!("{t}@{}", m.render()),
                (Some(t), None) => t,
                // No profile readable, but we know what it is running:
                // better than an empty corner, which reads as "nothing
                // bound here".
                (None, Some(m)) => m.render(),
                // Neither readable — but the badge must not go empty
                // on a bound session.  The core reads "this pane has a
                // badge" as "the cc plugin owns this pane" and uses it
                // to turn on the link scanner's fixed-width hard-wrap
                // merge; an empty string clears the entry, and a
                // wrapped path in this pane would quietly stop being
                // clickable.  Two characters is the price of keeping
                // that signal true.
                (None, None) => "cc".to_string(),
            };
            let project_basename = f
                .cwd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            // The whole path, for the resume line: where the session
            // lives is not the same question as what to call it.
            let project_dir = f.cwd.to_str().map(str::to_string);
            // cc-layer status.  The generic layer already said a job
            // owns this tty (that is how we found claude at all); this
            // says what claude is doing inside it.  A tool that shells
            // out appears as a process under claude younger than the
            // record that asked for it, which is what separates "the
            // tool is running" from "claude is parked on the approval
            // prompt" — long-lived children (MCP servers, started with
            // the session) are older than the record and don't count.
            if let Some(path) = jsonl_path.as_ref() {
                let record_at = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let has_young_child = pidtree::descendants_of(f.claude_pid, &procs)
                    .iter()
                    .any(|d| d.start_unix >= record_at);
                new_activity.insert(
                    f.shelld_sid,
                    tail_activity(path, has_young_child),
                );
            }
            // One read of the tail answers two questions: whether the
            // session is waiting on a timer of its own, and whether the
            // account refused it.
            let tail = jsonl_path.as_ref().map(tail_window);
            new_vetoes.insert(
                f.shelld_sid,
                (
                    has_work_in_flight(f.claude_pid, &procs),
                    tail.as_deref().map(tail_mentions_own_timer).unwrap_or(false),
                ),
            );
            new_cpu.insert(
                f.shelld_sid,
                (
                    pidtree::subtree_cpu_time_ns(f.claude_pid, &procs),
                    SystemTime::now(),
                ),
            );
            new_mapping.insert(f.shelld_sid, badge);
            new_meta.insert(
                f.shelld_sid,
                BindMeta {
                    profile_num,
                    config_dir: f.config_dir.clone(),
                    uuid: sid_uuid,
                    claude_pid: f.claude_pid,
                    project_basename,
                    project_dir,
                    // The same tail, third question: has the account
                    // refused this session, and when.
                    refused_at: tail.as_deref().and_then(super::last_quota_refusal),
                    transcript_at: jsonl_path
                        .as_ref()
                        .and_then(|p| p.metadata().ok())
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                    has_transcript: jsonl_path.is_some(),
                },
            );
            // No log line here on purpose — `session.bound` is
            // transition-only.  Main side diffs `result.new_mapping`
            // against `self.last_mapping` and only logs the deltas;
            // otherwise we'd write 12 lines per 2 s tick in steady
            // state and drown the file.
        }

        // Bounded growth: both per-pane maps follow the panes.
        self.last_model_by_pane.retain(|sid, _| sessions_seen.contains(sid));
        self.banner_tried.retain(|sid, _| sessions_seen.contains(sid));
        // What the badge already knows, handed to the chooser as a
        // fact rather than re-derived from the badge's text.
        let new_models = self
            .last_model_by_pane
            .iter()
            .map(|(sid, m)| (*sid, m.model.clone()))
            .collect();
        ScanResult { new_mapping, new_meta, new_activity, new_models, new_cpu, new_vetoes, scanned_at, sessions_seen, log_lines }
    }

    /// Reverse-lookup: encoded project dir → newest known session
    /// (id + its jsonl path, so callers can tail per-session state
    /// like the active model).  Cheap scan over `seen`; a dozen
    /// projects active in practice.
    ///
    /// Two constraints make this a *guess with guardrails* rather than
    /// a free-for-all:
    ///
    /// * `claimed` — a uuid already bound to another pane is skipped.
    ///   Two panes cwd'd into one project used to receive the identical
    ///   badge, which is provably wrong for at least one of them
    ///   (2026-07-30: sessions 383 + 394 both in `qualcomm/insight`,
    ///   both badged `9e304c9a`, which argv shows belongs to 383).
    /// * `claude_start` — a session whose file has not been written
    ///   since this claude process started cannot be the session it is
    ///   writing.  Without this, a pane freshly `claude`d in a project
    ///   whose newest session is days old wears that dead session's
    ///   uuid.  No eligible candidate ⇒ no badge, which is the honest
    ///   answer until the pane's own session file appears.
    pub(super) fn session_for_project(
        &self,
        encoded_dir: &str,
        claimed: &std::collections::HashSet<String>,
        claude_start: SystemTime,
    ) -> Option<(String, PathBuf)> {
        let mut newest: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir != encoded_dir {
                continue;
            }
            if claimed.contains(&s.session_id) {
                continue;
            }
            if s.last_mtime < claude_start {
                continue;
            }
            match newest {
                Some((t, _)) if t >= s.last_mtime => {}
                _ => newest = Some((s.last_mtime, s)),
            }
        }
        newest.map(|(_, s)| (s.session_id.clone(), s.jsonl_path.clone()))
    }

    /// The session that has superseded `argv_uuid` in this project, if
    /// one has.
    ///
    /// **Why argv is not enough.** `claude --resume X` puts X in argv
    /// and leaves it there for the life of the process.  A `/clear`
    /// starts a different session in the same process; so does an
    /// in-session `/resume`.  Bind by argv alone and the pane keeps
    /// naming a transcript nobody is writing any more — the badge
    /// tails a dead file for its model, reclamation parks and resumes
    /// the wrong conversation, and switching profile brings back a
    /// session the user left hours ago (2026-08-11 report: `torajs`,
    /// argv `--resume f7a8a54b` while the live transcript was
    /// `e024458b`, 3 minutes newer and still growing).
    ///
    /// The only evidence available is which transcript is being
    /// appended to — claude closes the file between writes, so there
    /// is no descriptor to inspect.  So: the newest unclaimed session
    /// of this project, provided it has been written **since this
    /// claude started** (older ones belong to other runs) and is
    /// clearly newer than the argv one.
    ///
    /// `SUPERSEDE_MARGIN` keeps a pane from flip-flopping between two
    /// files touched in the same instant at startup; a real `/clear`
    /// leaves the old transcript untouched from then on, so the margin
    /// costs nothing there.
    ///
    /// Known limit: two panes on **one** project cannot be told apart
    /// this way — both see the same newest file.  The claim set hands
    /// it to the lower `shelld_sid` and the other keeps its argv, which
    /// is the same tie-break the guess path has always used.
    pub(super) fn successor_session(
        &self,
        encoded_dir: &str,
        claimed: &std::collections::HashSet<String>,
        claude_start: SystemTime,
        argv_uuid: &str,
    ) -> Option<String> {
        const SUPERSEDE_MARGIN: Duration = Duration::from_secs(5);
        let argv_mtime = self.session_by_uuid(argv_uuid).map(|s| s.last_mtime);
        let mut best: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir != encoded_dir
                || s.session_id == argv_uuid
                || claimed.contains(&s.session_id)
                || s.last_mtime < claude_start
            {
                continue;
            }
            if let Some(t) = argv_mtime
                && s.last_mtime < t + SUPERSEDE_MARGIN {
                    continue;
                }
            match best {
                Some((t, _)) if t >= s.last_mtime => {}
                _ => best = Some((s.last_mtime, s)),
            }
        }
        best.map(|(_, s)| s.session_id.clone())
    }

    /// Look a session up by uuid — the argv-authoritative path knows
    /// *which* session a pane owns but still needs its jsonl to tail
    /// the active model.  `None` for a session too new to have been
    /// scanned yet; the badge then carries no model until it is.
    pub(super) fn session_by_uuid(&self, uuid: &str) -> Option<&SessionInfo> {
        self.seen.values().find(|s| s.session_id == uuid)
    }
}

//! Fixtures the claudecode tests share: a home of their own, a host
//! that records what it was asked, and scans built by hand.

use super::*;

/// A `HOME` of the test's own, with the state root inside it, both
/// pointed at for as long as the test holds it.  Nothing a test
/// does through it reaches the person's `~/.claude*` or the
/// installed app's state.
///
/// `_env` is declared first so it is dropped first: the variables
/// go back before the directory they named is removed.
pub(super) struct TestHome {
    _env: EnvGuard,
    dir: TempDir,
}

impl TestHome {
    pub(super) fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let mut env = EnvGuard::new();
        env.set("HOME", dir.path())
            .set("MARSPOT_STATE_DIR", dir.join("state"));
        Self { _env: env, dir }
    }
}

impl std::ops::Deref for TestHome {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.dir.path()
    }
}

impl AsRef<Path> for TestHome {
    fn as_ref(&self) -> &Path {
        self.dir.path()
    }
}

/// A process id for an op that is only inspected, never run.
pub(super) fn fid(pid: i32) -> pidtree::ProcId {
    pidtree::ProcId { pid, start_us: 0 }
}

/// The policy threshold every idle test runs at: the shipped
/// default, stated here rather than read off the machine running
/// the suite — see `run_idle_policy_at`.
pub(super) const TEST_THRESHOLD: Duration = Duration::from_secs(30 * 60);

/// A `WorkerCtx` with nothing cached, for the model-half tests.
pub(super) fn ctx_for_test(projects_root: PathBuf) -> WorkerCtx {
    WorkerCtx {
        projects_root,
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    }
}

pub(super) fn tmpfile(content: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "claudecode-plugin-test-{}-{}.jsonl",
        std::process::id(),
        n
    ));
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

/// A live process whose argv[0] is `claude`, for the paths that
/// re-check the claude before acting.  One per test process: `cat`
/// on a pipe nobody writes, so it ends when this process does.
pub(super) fn live_claude() -> pidtree::ProcId {
    use std::os::unix::process::CommandExt;
    static CAT: std::sync::OnceLock<std::process::Child> = std::sync::OnceLock::new();
    let c = CAT.get_or_init(|| {
        std::process::Command::new("/bin/cat")
            .arg0("claude")
            .stdin(std::process::Stdio::piped())
            // it outlives the test by a moment; holding the test's
            // output would have the runner call that a leak
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    });
    pidtree::proc_id(c.id() as i32).unwrap()
}

/// Badges compare as what they would be drawn as.
pub(super) fn rendered(b: Option<ModelBadge>) -> Option<String> {
    b.map(|b| b.render())
}

/// Minimal `PaneSessionHost` for driving a PaneSession directly.
pub(super) struct FakePaneSessionHost {
    pub(super) sid: u64,
}

impl crate::plugins::PaneSessionHost for FakePaneSessionHost {
    fn shelld_session_id(&self) -> u64 {
        self.sid
    }
    fn end(&self) {}
    fn set_badge(&self, _text: &str) {}
    fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
}

/// A `PluginHost` that records what the plugin asked it to do.
/// Only the methods the idle policy touches do anything.
pub(super) struct FakeHost {
    pub(super) state_dir: PathBuf,
    pub(super) status: std::sync::Mutex<HashMap<u64, crate::plugins::PaneStatusView>>,
    pub(super) begun: std::sync::Mutex<Vec<u64>>,
    /// The sessions handed over, kept so a test can tick them.
    /// The real host does exactly this; dropping them on the floor
    /// made the reclamation look synchronous, which it is not.
    pub(super) sessions: std::sync::Mutex<Vec<Box<dyn crate::plugins::PaneSession>>>,
    /// The host owns the op queue in production, so the fake does
    /// too — a test that pumped a plugin-private queue would be
    /// testing a shape that no longer exists.
    pub(super) ops: std::sync::Mutex<pty_op::PtyOps>,
    /// Every op handed over, as (session, step labels).  The queue
    /// itself runs on wall-clock deadlines, so a test that wants to
    /// know WHAT was asked for — rather than wait for it — reads
    /// this.
    pub(super) submitted: std::sync::Mutex<Vec<(u64, Vec<&'static str>)>>,
    /// What each op would write into the pane, as (session, bytes
    /// of every `Send` step joined).  The sentence a moved pane is
    /// told now travels on claude's command line, so asserting it
    /// means reading the bytes, not a step label.
    pub(super) sent: std::sync::Mutex<Vec<(u64, String)>>,
    /// Every badge / title push, in order.
    pub(super) pane_badges: std::sync::Mutex<Vec<(u64, String)>>,
    pub(super) pane_titles: std::sync::Mutex<Vec<(u64, String)>>,
}

impl FakeHost {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self::with_io(dir, Arc::new(ShelldClient::new(None)) as Arc<dyn pty_op::PtyIo>)
    }

    pub(super) fn with_io(dir: PathBuf, io: Arc<dyn pty_op::PtyIo>) -> Self {
        Self {
            state_dir: dir,
            status: std::sync::Mutex::new(HashMap::new()),
            begun: std::sync::Mutex::new(Vec::new()),
            sessions: std::sync::Mutex::new(Vec::new()),
            ops: std::sync::Mutex::new(pty_op::PtyOps::new(io)),
            submitted: std::sync::Mutex::new(Vec::new()),
            sent: std::sync::Mutex::new(Vec::new()),
            pane_badges: std::sync::Mutex::new(Vec::new()),
            pane_titles: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Start what was submitted, as the supervisor loop does.
    pub(super) fn pump_ops(&self) {
        let mut ops = self.ops.lock().unwrap();
        ops.pump(self);
    }
    pub(super) fn set_status(&self, sid: u64, view: crate::plugins::PaneStatusView) {
        self.status.lock().unwrap().insert(sid, view);
    }
}

impl PluginHost for FakeHost {
    fn set_pane_badge(
        &self,
        shelld_session_id: u64,
        text: &str,
    ) -> Result<(), PluginError> {
        self.pane_badges
            .lock()
            .unwrap()
            .push((shelld_session_id, text.to_string()));
        Ok(())
    }
    fn set_pane_title(
        &self,
        shelld_session_id: u64,
        text: &str,
    ) -> Result<(), PluginError> {
        self.pane_titles
            .lock()
            .unwrap()
            .push((shelld_session_id, text.to_string()));
        Ok(())
    }
    fn pane_status(
        &self,
        sid: u64,
    ) -> Result<Option<crate::plugins::PaneStatusView>, PluginError> {
        Ok(self.status.lock().unwrap().get(&sid).cloned())
    }
    fn state_dir(&self) -> Result<PathBuf, PluginError> {
        Ok(self.state_dir.clone())
    }
    fn log(&self, _: LogLevel, _: &str, _: &str) {}
    fn submit_pty_op_at(
        &self,
        sid: u64,
        op: pty_op::PtyOp,
        start_at: usize,
    ) -> Result<(), PluginError> {
        self.submitted
            .lock()
            .unwrap()
            .push((sid, op.steps.iter().map(|s| s.label).collect()));
        let sent: Vec<String> = op
            .steps
            .iter()
            .filter_map(|s| match &s.kind {
                pty_op::StepKind::Send(bytes) => {
                    Some(String::from_utf8_lossy(bytes).into_owned())
                }
                _ => None,
            })
            .collect();
        self.sent.lock().unwrap().push((sid, sent.join("\n")));
        self.ops
            .lock()
            .unwrap()
            .submit_at(sid, op, start_at)
            .map(|_| ())
            .ok_or_else(|| PluginError::Other("queue full".into()))
    }

    fn begin_pane_session(
        &self,
        sid: u64,
        _session: Box<dyn crate::plugins::PaneSession>,
    ) -> Result<(), PluginError> {
        self.begun.lock().unwrap().push(sid);
        self.sessions.lock().unwrap().push(_session);
        Ok(())
    }
}

/// Build a process the policy will accept as claude, and that can
/// be observed dying.
///
/// The guard reads `proc_cmdline`, i.e. **argv**, so what matters
/// is argv[0] — not the executable's name.  Measured on a live
/// claude: `ps comm` (argv[0]) is `claude` while `pbi_comm` (the
/// executable file) is the version string `2.1.220`, so argv is
/// the field with the program's identity in it.  A wrapper script
/// would produce argv[0] = `/bin/sh` and be rejected, which is
/// correct behaviour and useless as a fixture; `arg0` gives the
/// real shape instead.
pub(super) fn spawn_fake_claude(_dir: &std::path::Path) -> ChildGuard {
    use std::os::unix::process::CommandExt;
    ChildGuard::new(
        std::process::Command::new("/bin/sleep")
            .arg0("claude")
            .arg("120")
            .spawn()
            .unwrap(),
    )
}

pub(super) fn idle_view(held: Duration) -> crate::plugins::PaneStatusView {
    crate::plugins::PaneStatusView {
        status: marspot::pane_state::PaneStatus::AwaitingUser,
        held,
        quiescent: true,
    }
}

/// The status-line hook's record for `uuid`, naming Opus 5.5 at
/// medium effort, so the pane counts as one whose model is known.
///
/// The record lives under `MARSPOT_STATE_DIR`, so the calling test
/// has to hold a state root of its own; one that does not fails
/// here rather than writing into whatever root the process has.
pub(super) fn record_known_model(uuid: &str) {
    let push = model_push_dir();
    assert!(
        push.starts_with(std::env::temp_dir())
            && push.to_string_lossy().contains("/marspot-test-"),
        "the test does not hold a state root of its own: {}",
        push.display()
    );
    fs::create_dir_all(&push).unwrap();
    fs::write(push.join(uuid), "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n").unwrap();
}

pub(super) fn scan_with(sid: u64, claude_pid: i32, uuid: &str) -> ScanResult {
    if !uuid.is_empty() {
        record_known_model(uuid);
    }
    let mut new_meta = HashMap::new();
    new_meta.insert(
        sid,
        BindMeta {
            bound_by: "test",
            profile_num: 2,
            config_dir: Some("/Users/x/.claude-profile-2".into()),
            uuid: uuid.to_string(),
            claude: pidtree::proc_id(claude_pid).unwrap_or(fid(claude_pid)),
            project_basename: "proj".into(),
        project_dir: None,
        refused_at: None,
            // Long enough ago that the transcript clock is not what
            // any of these tests are about.
            transcript_at: SystemTime::now() - Duration::from_secs(7200),
            has_transcript: true,
        },
    );
    let mut new_cpu = HashMap::new();
    new_cpu.insert(sid, (1_000u64, SystemTime::now()));
    let choices = quota_inputs(&new_meta).1;
    ScanResult {
        new_mapping: HashMap::new(),
        new_models: HashMap::from([(sid, "opus-5-5".to_string())]),
        new_meta,
        new_activity: HashMap::new(),
        new_cpu,
        new_vetoes: HashMap::from([(sid, (false, false))]),
        agent_refused: HashMap::new(),
        scanned_at: SystemTime::now(),
        sessions_seen: vec![sid],
        log_lines: Vec::new(),
        profiles: Vec::new(),
        usage: None,
        choices,
    }
}

/// A temp `HOME` with two profiles: P1 on an account that is out,
/// P2 on one with room.  One right answer, so a test never has to
/// argue about which way the pick went.
pub(super) fn quota_home(tag: &str) -> TestHome {
    let home = TestHome::new(&format!("quota-{tag}"));
    for n in [1u8, 2] {
        fs::create_dir_all(home.join(format!(".claude-profile-{n}/sessions"))).unwrap();
        fs::write(
            home.join(format!(".claude-profile-{n}/.claude.json")),
            format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{tag}{n}@example\"\n  }}\n}}"),
        )
        .unwrap();
    }
    fs::create_dir_all(home.join(".local/state/devops")).unwrap();
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-09-29T00:00:00+00:00", "accounts": [
                   {{"name":"A","email":"{tag}1@example","status":"rejected","utilization_5h":0.0,
                    "utilization_7d":1.0,"reset_5h":1,"reset_7d":4102444800}},
                   {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":0.0,
                    "utilization_7d":0.2,"reset_5h":1,"reset_7d":4102444900}}]}}"#
        ),
    )
    .unwrap();
    // The status-line hook's records, which a switch starts the new
    // claude from: every pane in these tests is on Opus 5.5.
    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();
    for n in 77..=81 {
        fs::write(
            push.join(format!("u-{n}")),
            "opus-5-5\n/t/x.jsonl\nmedium\npids=1\nid=claude-opus-5-5\n",
        )
        .unwrap();
    }
    home
}

/// A pane bound to P1 of that `HOME`, with the session name it
/// would have to resume.
pub(super) fn pane_on_p1(home: &std::path::Path, uuid: &str) -> BindMeta {
    BindMeta {
        bound_by: "test",
        profile_num: 1,
        config_dir: Some(home.join(".claude-profile-1").to_string_lossy().into_owned()),
        uuid: uuid.into(),
        claude: live_claude(),
        has_transcript: true,
        transcript_at: SystemTime::UNIX_EPOCH,
        project_basename: String::new(),
        project_dir: None,
        refused_at: None,
    }
}

pub(super) fn scan_of(sid: u64, activity: CcActivity) -> ScanResult {
    let mut result = ScanResult {
        new_mapping: HashMap::new(),
        new_models: HashMap::new(),
        new_meta: HashMap::new(),
        new_activity: HashMap::new(),
        new_cpu: HashMap::new(),
        new_vetoes: HashMap::new(),
        agent_refused: HashMap::new(),
        scanned_at: SystemTime::now(),
        sessions_seen: vec![sid],
        log_lines: Vec::new(),
        profiles: profiles::ProfileCache::default().read(),
        usage: None,
        choices: HashMap::new(),
    };
    result.new_activity.insert(sid, activity);
    result.new_vetoes.insert(sid, (false, false));
    result.new_models.insert(sid, "opus-5-5".into());
    result
}

/// The sweep on a scan that bound what `plugin.last_meta` holds:
/// the sweep reads the scan's bindings, these tests set the plugin's.
pub(super) fn sweep(plugin: &mut ClaudecodePlugin, host: &FakeHost, scan: &mut ScanResult) {
    for (sid, m) in &plugin.last_meta {
        scan.new_meta.insert(*sid, m.clone());
    }
    // what the worker reads alongside the bindings
    (scan.usage, scan.choices) = quota_inputs(&scan.new_meta);
    plugin.sweep_unusable_profiles(host, scan);
}

pub(super) fn stranded_plugin(meta: BindMeta, sid: u64) -> ClaudecodePlugin {
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));
    plugin.last_meta.insert(sid, meta);
    plugin
}

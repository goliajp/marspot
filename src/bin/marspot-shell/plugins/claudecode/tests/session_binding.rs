//! Which session a pane is bound to, and which claude a hook record belongs to.

use super::*;

/// The fence moves only when the owning process is replaced.
/// argv is the only authoritative statement of which session a
/// pane owns; these are the shapes claude actually emits (captured
/// from a live 18-pane session on 2026-07-30).
#[test]
fn argv_uuid_reads_the_shapes_claude_emits() {
    // Plain resume: continues writing the file it names.
    assert_eq!(
        flag_uuid(
            "claude --resume 9e304c9a-93d3-428d-a6a6-fc537db59dc7",
            "--resume"
        )
        .as_deref(),
        Some("9e304c9a-93d3-428d-a6a6-fc537db59dc7")
    );
    // Forked resume: `--resume` names the *source* file by path,
    // `--session-id` names the session actually being written.
    let forked = "/x/ClaudeCode.app/Contents/MacOS/claude --bg-pty-host /tmp/s.sock 72 56 \
         -- /x/versions/2.1.220 --session-id 2c78740f-4a41-4e6e-8b99-5e0614326d1f \
         --fork-session --resume /Users/d/.claude-profile-1/projects/-Users-d-p/\
         429c7c04-81cc-43bf-8cbc-814dded894d0.jsonl --reply-on-resume";
    assert_eq!(
        flag_uuid(forked, "--session-id").as_deref(),
        Some("2c78740f-4a41-4e6e-8b99-5e0614326d1f")
    );
    assert_eq!(
        flag_uuid(forked, "--resume").as_deref(),
        Some("429c7c04-81cc-43bf-8cbc-814dded894d0"),
        "path form reduces to the bare uuid"
    );
    // A fresh interactive claude states nothing — that is what
    // keeps the inference path load-bearing.
    assert_eq!(flag_uuid("claude", "--resume"), None);
    assert_eq!(flag_uuid("claude --resume", "--resume"), None);
    // `--resume` also takes a session *name*; a non-uuid value must
    // not become a badge.
    assert_eq!(flag_uuid("claude --resume my-branch-work", "--resume"), None);
    assert!(!is_uuid("9e304c9a93d3428da6a6fc537db59dc7"), "dashes required");
    assert!(!is_uuid("9e304c9a-93d3-428d-a6a6-fc537db59dc7-extra"));
    assert!(!is_uuid("9e304c9z-93d3-428d-a6a6-fc537db59dc7"), "hex only");
}

/// Build a `WorkerCtx` whose `seen` holds one session per project.
fn ctx_with_sessions(sessions: &[(&str, &str, SystemTime)]) -> WorkerCtx {
    let mut seen = HashMap::new();
    for (uuid, project, mtime) in sessions {
        let path = PathBuf::from(format!("/fake/{project}/{uuid}.jsonl"));
        seen.insert(
            path.clone(),
            SessionInfo {
                session_id: (*uuid).to_string(),
                project_dir: (*project).to_string(),
                jsonl_path: path,
                last_mtime: *mtime,
                last_size: 1,
                has_conversation: true,
            },
        );
    }
    WorkerCtx {
        statusline_state: None,
        projects_root: PathBuf::from("/fake"),
        caches: Default::default(),
        seen,
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    }
}

/// The 2026-07-30 report: two panes cwd'd into one project both
/// wore badge `9e304c9a`.  One session belongs to one pane — the
/// second pane must come away with nothing, not a copy.
#[test]
fn a_session_already_bound_is_not_handed_to_a_second_pane() {
    let now = SystemTime::now();
    let ctx = ctx_with_sessions(&[("9e304c9a", "-Users-d-insight", now)]);
    let started = now - Duration::from_secs(600);
    let mut claimed = std::collections::HashSet::new();

    let first = ctx.session_for_project("-Users-d-insight", &claimed, started);
    assert_eq!(first.as_ref().map(|(u, _)| u.as_str()), Some("9e304c9a"));
    claimed.insert("9e304c9a".to_string());

    assert!(
        ctx.session_for_project("-Users-d-insight", &claimed, started).is_none(),
        "the only session is taken; a second pane gets no badge"
    );
}

/// A pane freshly `claude`d in a project whose newest session is
/// days old must not wear that dead session's uuid: the file has
/// not been written since this process started, so it cannot be
/// what this process is writing.
#[test]
fn a_session_older_than_the_process_is_not_bound() {
    let now = SystemTime::now();
    let stale = now - Duration::from_secs(2 * 24 * 3600);
    let ctx = ctx_with_sessions(&[("df58a571", "-Users-d-insight", stale)]);
    let claimed = std::collections::HashSet::new();

    let started_after = now - Duration::from_secs(300);
    assert!(
        ctx.session_for_project("-Users-d-insight", &claimed, started_after).is_none(),
        "nothing written since launch ⇒ no session of ours yet"
    );

    // Same session, a claude that predates it: now it is plausibly
    // ours and binds as before.
    let started_before = stale - Duration::from_secs(60);
    assert_eq!(
        ctx.session_for_project("-Users-d-insight", &claimed, started_before)
            .map(|(u, _)| u),
        Some("df58a571".to_string())
    );
}

/// `seen` is bounded by the projects that currently have panes ×
/// `SESSIONS_KEPT_PER_PROJECT`, and it *shrinks* — it used to be
/// insert-only, which meant a shell running for weeks accumulated an
/// entry per session file it had ever noticed.
#[test]
fn seen_keeps_the_newest_few_per_live_project_and_prunes_the_rest() {
    let tmp = TempDir::new("cc-refresh-seen");
    let root = tmp.path().to_path_buf();
    let mk = |project: &str, n: usize| {
        let dir = root.join(project);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..n {
            let uuid = format!("{:08x}-1111-2222-3333-444444444444", i);
            let p = dir.join(format!("{uuid}.jsonl"));
            fs::write(&p, format!("{{\"sessionId\":\"{uuid}\"}}\n")).unwrap();
            // Stagger mtimes so "newest N" is well defined; index 0
            // is oldest.
            let t = filetime_plus(i as u64);
            set_mtime(&p, t);
        }
    };
    mk("-p-alpha", 6);
    mk("-p-beta", 2);

    let mut ctx = WorkerCtx {
        projects_root: root.clone(),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    let mut logs = Vec::new();
    // Two panes in alpha: the project is walked once, not twice.
    ctx.refresh_seen(
        [
            (root.as_path(), "-p-alpha"),
            (root.as_path(), "-p-beta"),
            (root.as_path(), "-p-alpha"),
        ]
        .into_iter(),
        &mut logs,
    );

    let alpha: Vec<_> = ctx
        .seen
        .values()
        .filter(|s| s.project_dir == "-p-alpha")
        .map(|s| s.session_id.clone())
        .collect();
    assert_eq!(
        alpha.len(),
        SESSIONS_KEPT_PER_PROJECT,
        "kept the newest few, not all six: {alpha:?}"
    );
    assert!(
        !alpha.iter().any(|id| id.starts_with("00000000")
            || id.starts_with("00000001")),
        "the two oldest must be the ones dropped: {alpha:?}"
    );
    assert_eq!(
        ctx.seen.values().filter(|s| s.project_dir == "-p-beta").count(),
        2,
        "a project with fewer files keeps all of them"
    );

    // Beta's pane goes away: its entries must leave the map.
    let mut logs2 = Vec::new();
    ctx.refresh_seen([(root.as_path(), "-p-alpha")].into_iter(), &mut logs2);
    assert_eq!(ctx.seen.len(), SESSIONS_KEPT_PER_PROJECT);
    assert!(ctx.seen.values().all(|s| s.project_dir == "-p-alpha"));

}

fn filetime_plus(secs: u64) -> SystemTime {
    // A fixed base well in the past keeps the ordering deterministic
    // regardless of when the test runs.
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
}

fn set_mtime(path: &Path, t: SystemTime) {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
    let times = [
        libc::timeval { tv_sec: secs as i64, tv_usec: 0 },
        libc::timeval { tv_sec: secs as i64, tv_usec: 0 },
    ];
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    let r = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
    assert_eq!(r, 0, "utimes failed for {}", path.display());
}

#[test]
fn model_cutoff_tracks_the_owning_pid() {
    let tmp = TempDir::new("cc-model-cutoff-test");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("s.jsonl");
    fs::write(&path, "x".repeat(100)).unwrap();

    let mut ctx = WorkerCtx {
        projects_root: dir.clone(),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };

    // First sighting: nothing is fenced — those records belong to
    // the process we are looking at.
    assert_eq!(ctx.model_cutoff_for(&path, 111), 0);
    // Same process, file grew: still unfenced.
    fs::write(&path, "x".repeat(200)).unwrap();
    assert_eq!(ctx.model_cutoff_for(&path, 111), 0);
    // Process replaced: everything written so far is fenced off.
    assert_eq!(ctx.model_cutoff_for(&path, 222), 200);
    // And it stays put while that process lives.
    fs::write(&path, "x".repeat(500)).unwrap();
    assert_eq!(ctx.model_cutoff_for(&path, 222), 200);

}

/// 2026-08-02 report: after a profile switch the badge lost its
/// model half and kept it lost.
///
/// The fence is right — the old process's records do not describe
/// the new one — but a resumed claude writes nothing that names a
/// model until it finishes a turn (`mode` / `permission-mode` are
/// what it does write), so between the switch and the session's
/// next answer there is nothing behind the fence to read.  On a
/// parked pane that is hours of a badge saying `P4` alone.
#[test]
fn a_switched_profile_keeps_showing_the_last_model_it_saw() {
    let tmp = TempDir::new("cc-model-carry-test");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("s.jsonl");
    let assistant =
        |m: &str| format!("{{\"role\":\"assistant\",\"model\":\"claude-{m}\",\"x\":1}}\n");
    fs::write(&path, assistant("fable-5")).unwrap();

    let mut ctx = WorkerCtx {
        projects_root: dir.clone(),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    assert_eq!(rendered(ctx.model_for(&path, 111, 0)).as_deref(), Some("fable-5"));

    // The switch: new pid, so the fence lands at end of file — and
    // the startup records a resumed process writes name no model.
    use std::io::Write;
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, "{{\"type\":\"mode\",\"mode\":\"default\"}}").unwrap();
    writeln!(f, "{{\"type\":\"permission-mode\"}}").unwrap();
    drop(f);
    assert_eq!(
        rendered(ctx.model_for(&path, 222, 0)).as_deref(),
        Some("fable-5"),
        "the badge keeps what it last saw rather than going blank"
    );

    // …and yields the moment the new process actually says so.
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(f, "{}", assistant("opus-5")).unwrap();
    drop(f);
    assert_eq!(rendered(ctx.model_for(&path, 222, 0)).as_deref(), Some("opus-5"));

    // A session that has never named one has nothing to show, and
    // nothing is invented for it.
    let fresh = dir.join("fresh.jsonl");
    fs::write(&fresh, "{\"type\":\"mode\"}\n").unwrap();
    assert_eq!(ctx.model_for(&fresh, 333, 0), None);

}

#[test]
fn tail_model_prefers_newest_record_in_file() {
    // assistant(fable) then a later /model switch (the modern
    // "type":"user" record shape, JSON-escaped ANSI, trailing
    // "and saved..." remark) -> the switch wins.
    let path = tmpfile(concat!(
        r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to \u001b[1mOpus 4.8\u001b[22m and saved as your default for new sessions</local-command-stdout>"}}"#,
        "\n",
    ));
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("opus-4-8"));

    // ...and vice versa: an assistant turn after the switch wins.
    // (older "system"/"local_command" record shape)
    let path = tmpfile(concat!(
        r#"{"type":"system","subtype":"local_command","content":"<local-command-stdout>Kept model as \u001b[1mOpus 4.8\u001b[22m</local-command-stdout>"}"#,
        "\n",
        r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
        "\n",
    ));
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));

    // No model anywhere -> None.
    let path = tmpfile(r#"{"type":"user","text":"hi"}"#);
    assert_eq!(tail_model_short(&path, 0), None);

    // Self-reference guard: a conversation that DISCUSSES the
    // marker (e.g. this feature being developed in a marspot
    // session) stores it inside a text field with the
    // surrounding quotes JSON-escaped — must NOT be picked up.
    let path = tmpfile(concat!(
        r#"{"type":"message","role":"assistant","model":"claude-fable-5","content":[]}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"marker 改锚 <local-command-stdout>Set model to 之类的-块),而 我 5"}]}}"#,
        "\n",
    ));
    assert_eq!(rendered(tail_model_short(&path, 0)).as_deref(), Some("fable-5"));
}

#[test]
fn parse_session_id_finds_uuid_in_first_record() {
    let path = tmpfile(
        r#"{"type":"mode","mode":"normal","sessionId":"3ad170c8-7d00-4764-9bb8-fc31eba2154d"}
{"type":"user","text":"hello"}
"#,
    );
    let id = parse_session_id(&path).unwrap();
    assert_eq!(id, "3ad170c8-7d00-4764-9bb8-fc31eba2154d");
}

#[test]
fn parse_session_id_returns_none_when_missing() {
    let path = tmpfile(r#"{"type":"user","text":"hello"}"#);
    assert!(parse_session_id(&path).is_none());
}

/// The record the hook leaves is found by the pid of the claude
/// that ran it — the key that works before there is a transcript.
#[test]
fn the_hooks_record_is_found_by_which_claude_asked_for_it() {
    let _sandbox = sandbox_state_dir();
    let dir = model_push_dir();
    fs::create_dir_all(&dir).unwrap();
    let write = |uuid: &str, body: &str| fs::write(dir.join(uuid), body).unwrap();

    write("u-mine", "opus-5\n/t/u-mine.jsonl\nhigh\npids=9001,4242,1\n");
    write("u-theirs", "fable-5\n/t/u-theirs.jsonl\nhigh\npids=9002,7777,1\n");
    // Written before the hook recorded a chain: skipped, because a
    // record that cannot say which claude it belongs to must not
    // be guessed onto one.
    write("u-old", "sonnet-4.5\n/t/u-old.jsonl\nhigh\n");

    let got = pushed_session_for_pid(4242).expect("the claude that ran the hook");
    assert_eq!(got.0, "u-mine");
    assert_eq!(got.2.render(), "opus-5\u{b7}high");
    assert_eq!(got.1, PathBuf::from("/t/u-mine.jsonl"));

    assert!(
        pushed_session_for_pid(4243).is_none(),
        "a pid in no chain matches nothing",
    );
}

/// A claude inside a claude: both records name the outer pid, and
/// the outer pane must take the one whose hook ran nearest to it.
#[test]
fn the_nearer_claude_in_the_chain_wins() {
    let _sandbox = sandbox_state_dir();
    let dir = model_push_dir();
    fs::create_dir_all(&dir).unwrap();
    // The outer claude is 4242.  Its own hook sits one hop from
    // it; a subagent's hook sits three, through its own claude.
    fs::write(dir.join("u-outer"), "opus-5\n/t/u-outer.jsonl\nhigh\npids=5000,4242,1\n")
        .unwrap();
    fs::write(dir.join("u-inner"), "fable-5\n/t/u-inner.jsonl\nhigh\npids=6000,5900,4242,1\n")
        .unwrap();

    let got = pushed_session_for_pid(4242).expect("matched");
    assert_eq!(got.0, "u-outer", "the nearer hook is this pane's own");
}

/// The chain the hook writes starts at its own parent and walks
/// up — claude is somewhere above it, however many shells deep it
/// is invoked.
#[test]
fn the_recorded_chain_starts_at_this_process_parent() {
    let chain = ancestor_pids(8);
    assert!(!chain.is_empty(), "a process always has a parent");
    assert_eq!(chain[0], unsafe { libc::getppid() });
    assert!(chain.iter().all(|&p| p > 1), "{chain:?}");
    assert!(chain.len() <= 8, "{chain:?}");
}

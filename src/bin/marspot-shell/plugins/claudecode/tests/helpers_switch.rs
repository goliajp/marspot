//! Helpers refused by their account move by the allocation's rule for
//! helpers, and each move is reported for the helpers' record.

use super::*;
use super::super::with_helpers::{drain_account_changes, AccountChanged, HelperRoles};

/// P1 refuses; P2 is half used, P3 has the most left; both renew so far
/// ahead that they read as a week away, P2 first.
fn three_accounts(tag: &str) -> TestHome {
    let home = quota_home(tag);
    fs::create_dir_all(home.join(".claude-profile-3/sessions")).unwrap();
    fs::write(
        home.join(".claude-profile-3/.claude.json"),
        format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{tag}3@example\"\n  }}\n}}"),
    )
    .unwrap();
    fs::write(
        home.join(".local/state/devops/claude-usage.json"),
        format!(
            r#"{{"generated_at": "2026-09-29T00:00:00+00:00", "accounts": [
                   {{"name":"A","email":"{tag}1@example","status":"rejected","utilization_5h":0.0,
                    "utilization_7d":1.0,"reset_5h":1,"reset_7d":4102444800}},
                   {{"name":"B","email":"{tag}2@example","status":"allowed","utilization_5h":0.0,
                    "utilization_7d":0.5,"reset_5h":1,"reset_7d":4102444900}},
                   {{"name":"C","email":"{tag}3@example","status":"allowed","utilization_5h":0.0,
                    "utilization_7d":0.1,"reset_5h":1,"reset_7d":4102999900}}]}}"#
        ),
    )
    .unwrap();
    home
}

fn target(host: &FakeHost, i: usize) -> String {
    host.sent.lock().unwrap()[i].1.clone()
}

#[test]
fn helpers_refused_on_their_account_go_where_the_allocation_has_a_gap() {
    let home = three_accounts("helpers");
    let host = FakeHost::new(home.join("state"));
    let mut plugin = stranded_plugin(pane_on_p1(&home, "u-77"), 77);
    plugin.last_meta.insert(78, pane_on_p1(&home, "u-78"));
    plugin.last_meta.insert(79, pane_on_p1(&home, "u-79"));
    let mut result = scan_of(77, CcActivity::AwaitingUser);
    for sid in [78, 79] {
        result.sessions_seen.push(sid);
        result.new_activity.insert(sid, CcActivity::AwaitingUser);
        result.new_vetoes.insert(sid, (false, false));
        result.new_models.insert(sid, "opus-5-5".into());
    }
    // 77 and 78 are helpers of 50; 79 is an ordinary pane
    result.helpers = HelperRoles::of([(50, 0), (77, 50), (78, 50), (79, 0)]);
    drain_account_changes();
    sweep(&mut plugin, &host, &mut result);

    let mut moved: Vec<u64> = host.submitted.lock().unwrap().iter().map(|(sid, _)| *sid).collect();
    moved.sort();
    assert_eq!(moved, [77, 78, 79]);
    let to = |sid: u64| {
        let i = host.sent.lock().unwrap().iter().position(|(s, _)| *s == sid).unwrap();
        target(&host, i)
    };
    // both renewals read as a week away, P2's first.  The ordinary pane,
    // first, to P2: all it uses there would lapse.  With it there P2's
    // week is used up before the renewal, so the first helper saves
    // nothing there and goes to P3, where all it uses would lapse; with
    // that helper on P3 neither account lapses any more, and the second
    // goes to the earlier renewal, P2
    let on = |sid: u64| if to(sid).contains(".claude-profile-2") { 2 } else if to(sid).contains(".claude-profile-3") { 3 } else { 0 };
    assert_eq!([on(79), on(77), on(78)], [2, 3, 2]);

    let mut changes = drain_account_changes();
    changes.sort_by_key(|c| c.sid);
    let got: Vec<(u64, &str, u8)> = changes.iter().map(|c: &AccountChanged| (c.sid, c.agent, c.from)).collect();
    assert_eq!(got, [(77, "claude", 1), (78, "claude", 1)], "only the helpers' moves are reported");
    assert_eq!(changes.iter().map(|c| c.to).collect::<Vec<_>>(), [3, 2]);
}

/// The next scan after claude writes that its turn is over says so.
#[test]
fn helpers_a_scan_reads_the_end_of_a_turn_claude_recorded() {
    use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
    use super::super::with_helpers::turns;
    use super::idle_loop::{KillPaneTree, drain_pty};

    // as the whole idle loop's test: it needs a real pty and process tree
    if std::env::var_os("MARSPOT_PTY_INTEGRATION").is_none() {
        eprintln!("SKIPPED: set MARSPOT_PTY_INTEGRATION=1 (bin/test.sh does)");
        return;
    }
    let root = TestHome::new("turn-end");
    let (proj, profile) = (root.join("proj"), root.join(".claude"));
    fs::create_dir_all(&proj).unwrap();
    fs::create_dir_all(profile.join("sessions")).unwrap();
    std::os::unix::fs::symlink("/bin/sleep", root.join("claude")).unwrap();
    let proj = fs::canonicalize(&proj).unwrap();
    let transcripts = profile.join("projects").join(encode_project_dir(&proj));
    fs::create_dir_all(&transcripts).unwrap();

    let pty = Pty::spawn(PtyConfig {
        program: "/bin/zsh".into(),
        args: vec!["-f".into()],
        size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
        argv0: None,
        cwd: Some(proj.to_string_lossy().into_owned()),
        env_remove_prefixes: marspot_term::pty::SESSION_ENV_PREFIXES.iter().map(|s| (*s).to_string()).collect(),
        env_set: Vec::new(),
    })
    .expect("spawn zsh");
    let shell = pty.child_pid();
    let _tree = KillPaneTree(shell);
    let master = pty.raw_master();
    // SAFETY: flags of a descriptor this test owns
    unsafe { libc::fcntl(master, libc::F_SETFL, libc::fcntl(master, libc::F_GETFL) | libc::O_NONBLOCK) };
    let start = format!("'{}' 600\r", root.join("claude").display());
    // SAFETY: the buffer outlives the call
    unsafe { libc::write(master, start.as_ptr().cast(), start.len()) };
    let deadline = Instant::now() + Duration::from_secs(60);
    let claude = loop {
        // a shell setting up its terminal waits for what it wrote to be read
        drain_pty(master);
        let found = pidtree::descendants_of(shell, &pidtree::list_all_procs()).into_iter().find(looks_like_claudecode);
        if let Some(c) = found {
            break c;
        }
        assert!(Instant::now() < deadline, "the stand-in claude never ran");
        std::thread::sleep(Duration::from_millis(50));
    };
    let uuid = "bbbbbbbb-1111-2222-3333-444444444444";
    let record = format!(r#"{{"pid":{},"sessionId":"{uuid}","kind":"interactive"}}"#, claude.pid);
    fs::write(profile.join(format!("sessions/{}.json", claude.pid)), record).unwrap();
    let entry = marspot_term::session_registry::SessionEntry {
        id: 1,
        pid: std::process::id() as i32,
        socket: root.join("s.sock"),
        cols: 80,
        rows: 24,
        title: "t".into(),
        cwd: proj.to_string_lossy().into_owned(),
        proto_version: marspot_term::shell_proto::PROTO_VERSION,
        created_at_unix: 0,
        shm_name: String::new(),
        shell_child_pid: shell,
        build: String::new(),
        stamps_regions: false,
        shadow_of: 7,
    };
    let mut worker = WorkerCtx {
        projects_root: root.join(".claude").join("projects"),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    let mut transcript = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}},\"timestamp\":\"2026-11-23T15:14:52.000Z\",\"sessionId\":\"{uuid}\"}}\n\
         {{\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}],\"stop_reason\":\"end_turn\"}},\"type\":\"assistant\"}}\n"
    );
    let mut scan = |text: &str| {
        fs::write(transcripts.join(format!("{uuid}.jsonl")), text).unwrap();
        let mut argv = crate::observe::ArgvCache::default();
        let table = pidtree::ProcTable::take();
        let obs = crate::observe::Observation::from_parts(Instant::now(), table, vec![(entry.clone(), true)], |r| argv.get(r));
        let r = worker.scan_once(&obs);
        assert_eq!(r.new_meta.get(&1).map(|m| m.bound_by), Some("record"));
        turns::look(1)
    };
    assert_eq!(scan(&transcript), Some(turns::Look::NotOver), "its Stop hooks have not run");
    transcript.push_str("{\"type\":\"system\",\"subtype\":\"turn_duration\",\"durationMs\":5,\"timestamp\":\"2026-11-23T15:14:57.635Z\"}\n");
    assert_eq!(scan(&transcript), Some(turns::Look::Over { began: Some(1_795_446_892_000), at: 1_795_446_897_635, quiet_after: None }));
    transcript.push_str("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"more\"}}\n");
    assert_eq!(scan(&transcript), Some(turns::Look::NotOver), "the next turn");
}

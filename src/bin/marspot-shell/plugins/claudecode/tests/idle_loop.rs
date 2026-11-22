//! The whole idle loop, end to end, on a PTY the test owns.

use super::*;

/// Drain a PTY master (set non-blocking by the caller) into a
/// string, so the test can see what the shell echoed.
fn drain_pty(master: std::os::fd::RawFd) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe {
            libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn poll_until<F: FnMut() -> bool>(what: &str, mut f: F) {
    // Sixty, not ten.  What is being waited for is a process
    // appearing and a scan noticing it; how long that takes is a
    // property of the machine, and ten seconds was enough here and
    // not enough on a shared build runner.  The budget only has to
    // be long enough that exceeding it means something is actually
    // wrong.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Hibernate → dormant → keypress → resume, end to end.
#[test]
fn the_whole_idle_loop_runs_on_a_pty_this_test_owns() {
    // This one drives a real pseudo-terminal, types into a shell,
    // waits for a process to appear in the tree and then reads the
    // transcript it writes.  It is shaped like the machines this
    // project is developed on, and on a shared build runner it
    // binds the pane and then reports no activity for it — a
    // difference in the environment, not in the code, and not one
    // worth weakening the test to paper over.
    //
    // So it is opt-in, and `bin/test.sh` opts in.  The gate still
    // runs it every time; CI says it skipped rather than pretending
    // to have run it.
    if std::env::var_os("MARSPOT_PTY_INTEGRATION").is_none() {
        eprintln!(
            "SKIPPED: needs a real pty and process tree — set \
             MARSPOT_PTY_INTEGRATION=1 (bin/test.sh does)"
        );
        return;
    }
    use marspot_term::pty::{Pty, PtyConfig, TerminalSize};

    // The transcript the scan reads lives under `$HOME/.claude`, so
    // the whole test runs under a `HOME` of its own.
    let root = TestHome::new("loop");
    let bin = root.join("bin");
    let proj = root.join("proj");
    let state = root.join("state");
    for d in [&bin, &proj, &state] {
        fs::create_dir_all(d).unwrap();
    }
    // A "claude" that is a real binary, so argv[0] ends in
    // `claude` — the identity both the scan and the pid guard
    // check.  A #! wrapper would give argv[0] = /bin/sh and be
    // (correctly) rejected.
    //
    // Symlink, not copy: a copied system binary is killed on exec
    // ("zsh: killed") because the signature no longer matches the
    // file it is being loaded from.  The symlink runs the original,
    // signed binary under the name we need.
    std::os::unix::fs::symlink("/bin/sleep", bin.join("claude")).unwrap();

    // The transcript the scan binds against: one finished assistant
    // turn, in the on-disk shape (`message` before `type`).
    let uuid = "aaaaaaaa-1111-2222-3333-444444444444";
    let home = root.to_path_buf();
    // Use the plugin's own encoder, and canonicalise first: the
    // scan encodes what `proc_cwd` reports, which is the resolved
    // path (`/private/var/...`), while `temp_dir()` hands back the
    // symlinked one (`/var/...`).  Encoding the wrong one puts the
    // fixture transcript in a directory the scan never looks at.
    let proj = fs::canonicalize(&proj).unwrap();
    let encoded = encode_project_dir(&proj);
    let proj_dir = home.join(".claude/projects").join(&encoded);
    fs::create_dir_all(&proj_dir).unwrap();
    let jsonl = proj_dir.join(format!("{uuid}.jsonl"));
    fs::write(
        &jsonl,
        format!(
            r#"{{"parentUuid":"p","isSidechain":false,"message":{{"model":"m","id":"i","type":"message","role":"assistant","content":[{{"type":"text","text":"done"}}]}},"type":"assistant","uuid":"u","sessionId":"{uuid}"}}"#
        ) + "\n",
    )
    .unwrap();

    // A real shell on a PTY this test owns.
    let pty = Pty::spawn(PtyConfig {
        program: "/bin/zsh".into(),
        args: vec!["-f".into()],
        size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
        argv0: None,
        cwd: Some(proj.to_string_lossy().into_owned()),
        env_remove_prefixes: marspot_term::pty::SESSION_ENV_PREFIXES
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        env_set: Vec::new(),
    })
    .expect("spawn zsh");
    let master = pty.raw_master();
    unsafe {
        let fl = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let shell_pid = pty.child_pid();
    // Declared after `pty`, so it runs first: whatever the pane
    // started goes before the shell does, pass or fail.
    let _pane_tree = KillPaneTree(shell_pid);

    // Registry entry, so the plugin's session list finds this pane
    // exactly as it finds a real one.
    marspot_term::session_registry::write_session_entry(
        &marspot_term::session_registry::SessionEntry {
            id: 1,
            pid: std::process::id() as i32,
            socket: state.join("s.sock"),
            cols: 80,
            rows: 24,
            title: "loop".into(),
            cwd: proj.to_string_lossy().into_owned(),
            proto_version: marspot_term::shell_proto::PROTO_VERSION,
            created_at_unix: 0,
            shm_name: String::new(),
            shell_child_pid: shell_pid,
            build: String::new(),
            stamps_regions: false,
        },
    )
    .unwrap();

    // Start the stand-in claude in the pane — by writing to the
    // master, which is what a keyboard is.
    // Put the stand-in on the pane's PATH before starting it: the
    // wake writes a bare `claude --resume …`, so PATH is what
    // decides whether this test drives the stand-in or launches a
    // real claude (it launched a real one until this line existed
    // — visible in the pane's output as the trust prompt).
    // A profile dir on the stand-in's environment, so the scan
    // reads it exactly as it reads a real one — and so the wake
    // has to carry it back.  Without it the policy now (rightly)
    // refuses to reclaim a session it cannot name the profile of.
    let profile_dir = root.join(".claude-profile-9");
    fs::create_dir_all(&profile_dir).unwrap();
    let start = format!(
        "export PATH={}:$PATH CLAUDE_CONFIG_DIR={}\rclaude 100000\r",
        bin.to_string_lossy(),
        profile_dir.to_string_lossy(),
    );
    unsafe {
        libc::write(
            master,
            start.as_ptr() as *const libc::c_void,
            start.len(),
        )
    };
    poll_until("the stand-in claude to be running", || {
        drain_pty(master);
        let procs = pidtree::list_all_procs();
        pidtree::descendants_of(shell_pid, &procs)
            .iter()
            .any(looks_like_claudecode)
    });
    // The scan only binds a transcript that is newer than the
    // process writing it, same as in production.
    fs::write(&jsonl, fs::read(&jsonl).unwrap()).unwrap();

    // Real scan → real binding.
    let mut worker = WorkerCtx {
        projects_root: home.join(".claude/projects"),
        caches: Default::default(),
        statusline_state: None,
        seen: HashMap::new(),
        model_cutoff: HashMap::new(),
        banner_tried: HashMap::new(),
        last_refusal: HashMap::new(),
        last_model: HashMap::new(),
        last_model_by_pane: HashMap::new(),
    };
    // the stand-in registry entry names this test process, which is
    // no L3; the session is marked live as the observer would mark ours
    let observe = || {
        let e = marspot_term::session_registry::read_session_entry(1).unwrap();
        let mut argv = crate::observe::ArgvCache::default();
        let table = pidtree::ProcTable::take();
        crate::observe::Observation::from_parts(std::time::Instant::now(), table, vec![(e, true)], |r| argv.get(r))
    };
    let mut scan = worker.scan_once(&observe());
    poll_until("the scan to bind the pane", || {
        scan = worker.scan_once(&observe());
        scan.new_meta.contains_key(&1)
    });
    let claude_pid = scan.new_meta[&1].claude.pid;
    assert_eq!(
        scan.new_activity.get(&1),
        Some(&marspot::pane_state::Activity::AwaitingUser),
        "a finished turn reads as awaiting the user"
    );

    // The real state machine, fed the real kernel observation.
    let mut machine = marspot::pane_state::PaneMachine::new(std::time::Instant::now());
    let base = std::time::Instant::now();
    for i in 1..=marspot::pane_state::CONFIRM_TICKS {
        machine.observe(
            marspot::pane_state::Observation {
                generic: pidtree::observe_pane(shell_pid),
                activity: marspot::pane_state::Activity::AwaitingUser,
                pty_quiet: true,
            },
            base + Duration::from_secs(i as u64),
        );
    }
    assert!(
        machine.quiescent(),
        "machine should call this pane quiet: {:?}",
        machine.status()
    );

    // Policy, with the pane presented as long-idle.
    let inject = Arc::new(PtyInject {
        master,
        sent: std::sync::Mutex::new(Vec::new()),
        holds: std::sync::Mutex::new(Vec::new()),
        mouse_resets: std::sync::Mutex::new(Vec::new()),
    });
    // The host owns the queue, so give it the route to this test's
    // PTY: otherwise the reclamation half would run against a
    // no-op client and the test would be watching a shadow.
    let host = FakeHost::with_io(
        state.clone(),
        Arc::new(ShelldClient::new(Some(inject.clone()))) as Arc<dyn pty_op::PtyIo>,
    );
    host.set_status(
        1,
        crate::plugins::PaneStatusView {
            status: machine.status().clone(),
            held: Duration::from_secs(7200),
            quiescent: true,
        },
    );
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(Some(inject.clone()))));

    // First pass records the CPU baseline; make it old enough for
    // the second to be able to decide.
    // The model and effort, as the status-line hook would have
    // recorded them: a pane whose model is not known is not reclaimed.
    record_known_model(uuid);
    let mut first = ScanResult {
        new_mapping: scan.new_mapping.clone(),
        new_models: HashMap::from([(1, "opus-5-5".to_string())]),
        new_meta: scan.new_meta.clone(),
        new_activity: scan.new_activity.clone(),
        new_cpu: HashMap::new(),
        new_vetoes: HashMap::from([(1, (false, false))]),
        agent_refused: HashMap::new(),
        fable_refused: HashMap::new(),
        scanned_at: SystemTime::now(),
        sessions_seen: vec![1],
        log_lines: Vec::new(),
        profiles: Vec::new(),
        usage: None,
        choices: HashMap::new(),
    };
    // The stand-in is a system binary, and macOS redacts the
    // environment of hardened ones — `proc_cmdline` works on it,
    // `proc_env_value` returns None (measured).  A real claude is
    // an ordinary user binary and reads fine, which is why the
    // badge shows P1/P2/P3 in production.  So the profile is
    // supplied here rather than pretending the scan could read it;
    // everything downstream of the binding is still the real path.
    for m in first.new_meta.values_mut() {
        m.profile_num = 9;
        m.config_dir = Some(profile_dir.to_string_lossy().into_owned());
        // The clock that decides is the transcript's age; this
        // fixture's session has been idle for hours.
        m.transcript_at = SystemTime::now() - Duration::from_secs(7200);
    }
    first.new_cpu.insert(
        1,
        (
            pidtree::subtree_cpu_time_ns(claude_pid, &pidtree::list_all_procs()),
            SystemTime::now() - Duration::from_secs(120),
        ),
    );
    plugin.run_idle_policy_at(&host, &first, TEST_THRESHOLD);
    assert!(plugin.dormant.is_empty(), "one sample decides nothing");

    let mut second = ScanResult {
        new_mapping: first.new_mapping.clone(),
        new_models: first.new_models.clone(),
        new_meta: first.new_meta.clone(),
        new_activity: first.new_activity.clone(),
        new_cpu: HashMap::new(),
        new_vetoes: HashMap::from([(1, (false, false))]),
        agent_refused: HashMap::new(),
        fable_refused: HashMap::new(),
        scanned_at: SystemTime::now(),
        sessions_seen: vec![1],
        log_lines: Vec::new(),
        profiles: Vec::new(),
        usage: None,
        choices: quota_inputs(&first.new_meta).1,
    };
    second.new_cpu.insert(
        1,
        (
            pidtree::subtree_cpu_time_ns(claude_pid, &pidtree::list_all_procs()),
            SystemTime::now(),
        ),
    );
    plugin.run_idle_policy_at(&host, &second, TEST_THRESHOLD);
    assert_eq!(plugin.dormant.len(), 1, "the pane should now be dormant");
    assert_eq!(
        plugin.dormant[0].config_dir.as_deref(),
        Some(profile_dir.to_string_lossy().as_ref()),
        "the parked record carries the profile the session ran under"
    );
    assert_eq!(plugin.dormant[0].uuid, uuid);

    // Nothing has run yet: the decision queues an op, the service
    // starts it on the next tick.  Both halves are production's,
    // and the test drives them in the same order.
    host.pump_ops();
    // The signal is sent from the run's own ticks, not from the
    // policy call: the pane's picture has to be held first, and
    // that request crosses two process boundaries while a signal
    // crosses none.  Drive the ticks the way the real host does.
    let ticking_host = FakePaneSessionHost { sid: 1 };
    let tick_all = || {
        for s in host.sessions.lock().unwrap().iter_mut() {
            s.on_tick(&ticking_host);
        }
    };
    // The signal was real: claude is gone from the pane.
    poll_until("the stand-in claude to be reclaimed", || {
        tick_all();
        let procs = pidtree::list_all_procs();
        !pidtree::descendants_of(shell_pid, &procs)
            .iter()
            .any(looks_like_claudecode)
    });

    // …and the shell is back in front of its own tty, which is
    // what makes the pane usable again.
    poll_until("the shell to take the tty back", || {
        drain_pty(master);
        matches!(
            pidtree::observe_pane(shell_pid),
            marspot::pane_state::Generic::Idle
        )
    });

    // Wake: focus, which is what the user actually does first.
    // (A keypress works too — same path — but waiting for one
    // starts the restore after they have already tried to use the
    // pane.)
    // The run the policy armed is this same script, parked.  Rebuild
    // it at the park step — which is exactly what a re-arm after an
    // L1 restart does — and drive the wake directly.
    let client = Arc::new(ShelldClient::new(Some(inject.clone())));
    let op = reclaim_op(uuid, Some(&profile_dir.to_string_lossy()), None, shell_pid, None, &CliChoice::default())
        .expect("a quotable profile builds a script");
    // By name, exactly as the re-arm does -- a test that hard-codes
    // the position is a test that stops asking the question when a
    // step is inserted, which is how this one found the bug.
    let park = op
        .index_of(idle::RECLAIM_PARK_LABEL)
        .expect("the script has a park step");
    let mut session = pty_op::OpRunner::new(
        op,
        Box::new(pty_op::RealEnv::new(client as Arc<dyn pty_op::PtyIo>)),
    )
    .start_at(park);
    let host_session = FakePaneSessionHost { sid: 1 };
    assert!(session.is_awaiting_user(), "a re-armed run parks at the wake");
    crate::plugins::PaneSession::on_focus(&mut session, &host_session);
    assert!(
        !session.is_awaiting_user(),
        "focusing a parked pane starts the restore"
    );
    // The host's loop drives a script forward, one step per tick:
    // the "has it come back by itself?" check, the interrupt that
    // empties the shell's line, the wait for its fresh prompt, the
    // line.  The prompt is seen in the pane's output, which a
    // session logs and this test logs for it.
    // under the state dir `TestHome` set
    let mut log = marspot_term::bytelog::ByteLog::open(1).expect("a bytelog for the pane");
    let mut shown = String::new();
    poll_until("the resume line to be typed", || {
        let out = drain_pty(master);
        log.append(out.as_bytes()).unwrap();
        log.flush();
        shown.push_str(&out);
        crate::plugins::PaneSession::on_tick(&mut session, &host_session);
        String::from_utf8_lossy(&inject.sent.lock().unwrap()).contains("--resume")
    });
    // A keystroke arriving mid-wake is swallowed rather than run
    // as a shell command.
    let handling = crate::plugins::PaneSession::on_user_key(
        &mut session,
        &host_session,
        &marspot::shell_proto::WireKeyEvent {
            state: marspot::shell_proto::WireKeyState::Pressed,
            mods: 0,
            kind: marspot::shell_proto::WireLogicalKind::Char,
            key_data: 'x' as u32,
            text: "x".into(),
            ..Default::default()
        },
    );
    assert!(
        matches!(handling, crate::plugins::KeyHandling::Swallow),
        "keys during the wake are consumed, not run as shell commands"
    );
    // In pieces, not as one string: the line carries a
    // `--settings` document naming this binary by absolute path,
    // which differs between a dev build, the bundle and
    // `binaries/current`.
    let sent = String::from_utf8_lossy(&inject.sent.lock().unwrap()).into_owned();
    // the interrupt that empties the shell's line comes first
    let sent = sent.strip_prefix('\x03').unwrap_or_else(|| panic!("the line is emptied first: {sent:?}"));
    assert!(
        sent.starts_with(&format!(
            "printf '\\033[H\\033[2J'; CLAUDE_CONFIG_DIR='{}' claude ",
            profile_dir.to_string_lossy()
        )),
        "the wake resumes under the profile it was running, behind a \
         screen wipe so the line itself never shows: {sent}"
    );
    assert!(sent.ends_with(&format!(" --resume {uuid}\r")), "{sent}");
    // It really reached the PTY: the shell echoes it back…
    let mut echoed = shown;
    poll_until("the pane to echo the resume line", || {
        echoed.push_str(&drain_pty(master));
        echoed.contains("--resume")
    });
    // …and the shell RUNS it.  Echo alone was what the first
    // version asserted, and it is not the same claim: the user
    // reported the line sitting on the prompt waiting for them to
    // press Enter, which an echo-only assertion cannot see.
    // `claude` here is the stand-in (a `sleep` symlink), so it
    // exits immediately on the unknown `--resume` argument — what
    // matters is that the shell dispatched the line at all, which
    // shows up as the prompt coming back after it.
    poll_until("the shell to execute the resume line", || {
        echoed.push_str(&drain_pty(master));
        // The stand-in is `sleep` under another name, so the
        // resume's arguments make it fail loudly — which is proof
        // the line was DISPATCHED, not left sitting on the prompt
        // for the user to press Enter on (the bug this asserts
        // against).
        echoed.contains("invalid time interval") || echoed.contains("usage:")
    });

}

/// Kills, when dropped, every process group a pane's shell has
/// started: a stand-in left running would outlive the test.
struct KillPaneTree(i32);

impl Drop for KillPaneTree {
    fn drop(&mut self) {
        for pid in pidtree::child_pids(self.0) {
            if let Some(row) = pidtree::proc_row(pid) {
                unsafe {
                    libc::killpg(row.pgid, libc::SIGCONT);
                    libc::killpg(row.pgid, libc::SIGKILL);
                }
            }
        }
    }
}

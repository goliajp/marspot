//! The idle reclamation policy: the evidence it weighs and the vetoes it honours.

use super::*;

// ── idle reclamation policy ───────────────────────────────────
// Every one of these is about NOT killing something.  The upside
// of hibernating is memory; the downside is a lost turn, a lost
// pending tool call, or a lost background task — so the tests are
// written from the veto side.

const HOUR: Duration = Duration::from_secs(3600);

const HALF_HOUR: Duration = Duration::from_secs(1800);

/// Evidence for a session that has been idle `secs` seconds — by
/// its own transcript, which is the clock that decides.
fn idle_for(secs: u64) -> IdleEvidence {
    IdleEvidence {
        idle_for: Duration::from_secs(secs),
        quiescent: true,
        awaiting_user: true,
        held: Duration::from_secs(secs),
        cpu_delta_ns: 0,
        since_sample: Duration::from_secs(120),
        user_here: false,
        work_in_flight: false,
        own_timer: false,
        helper_bound: false,
    }
}

/// A pane this plugin has parked reports `Dormant`, not `Absent`.
/// Both look the same from outside (no claude, shell at a prompt);
/// only one of them owes a restore, and the difference has to be
/// visible to anyone reading the pane's state.
#[test]
fn an_unbound_pane_reports_dormant_only_when_something_is_parked() {
    let parked = vec![DormantRecord {
        shelld_sid: 7,
        uuid: "u".into(),
        profile_num: 1,
            config_dir: None,
            project_dir: None,
            choice: CliChoice::default(),
        created_at: std::time::UNIX_EPOCH,
    }];
    assert_eq!(activity_for_unbound(7, &parked), CcActivity::Dormant);
    assert_eq!(activity_for_unbound(8, &parked), CcActivity::Absent);
    assert_eq!(activity_for_unbound(7, &[]), CcActivity::Absent);
    // …and the composed state keeps the two apart.
    assert!(
        marspot::pane_state::compose(
            &marspot::pane_state::Generic::Idle,
            CcActivity::Dormant,
            true,
        )
        .owes_restore()
    );
    assert!(
        !marspot::pane_state::compose(
            &marspot::pane_state::Generic::Idle,
            CcActivity::Absent,
            true,
        )
        .owes_restore()
    );
}

/// The threshold is the user's, so it comes from the settings
/// file — and the env var still overrides it, because the sandbox
/// scripts and soak tests set that and mean "this run", not "what
/// the user wants".
#[test]
fn the_reclaim_threshold_follows_the_settings_file() {
    let mut env = EnvGuard::new();
    env.remove("MARSPOT_CC_IDLE_HIBERNATE_S");

    marspot::settings::set_for_test(marspot::settings::Settings {
        reclaim_enabled: true,
        reclaim_idle_minutes: 45,
        reclaim_prefetch: true,
        ..marspot::settings::Settings::default()
    });
    assert_eq!(hibernate_after(), Some(Duration::from_secs(45 * 60)));

    // Both ways of saying never.
    marspot::settings::set_for_test(marspot::settings::Settings {
        reclaim_enabled: false,
        ..marspot::settings::Settings::default()
    });
    assert_eq!(hibernate_after(), None, "the switch");
    marspot::settings::set_for_test(marspot::settings::Settings {
        reclaim_idle_minutes: 0,
        ..marspot::settings::Settings::default()
    });
    assert_eq!(hibernate_after(), None, "the zero");

    // And the env var wins over any of it.
    env.set("MARSPOT_CC_IDLE_HIBERNATE_S", "60");
    assert_eq!(hibernate_after(), Some(Duration::from_secs(60)));
}

/// 2026-08-03 report: come back to marspot after a short break and
/// the first pane you touch takes seconds before it answers.
///
/// It had been reclaimed.  The clock that decides is the session's
/// transcript age, and that clock knows nothing about where the
/// user is — so the pane they were sitting in, reading the last
/// answer, was parked out from under them and the next keystroke
/// paid for a wake.  Invisibly, since a reclamation is deliberately
/// silent: it reads as marspot simply going unresponsive.
#[test]
fn the_pane_the_user_is_sitting_in_is_never_reclaimed() {
    // Idle for two hours by its own clock, and eligible on every
    // other count.
    let e = IdleEvidence { user_here: true, ..idle_for(7200) };
    assert!(!should_hibernate(e, HOUR));
    assert_eq!(
        blocking_reason(e, HOUR).map(|(cat, _)| cat),
        Some("user_here"),
        "and it says so, rather than looking like a policy that never fires"
    );

    // The same pane, once they move to another one.
    let gone = IdleEvidence { user_here: false, ..e };
    assert!(should_hibernate(gone, HOUR));
}

/// The reclamation gate excludes an already-parked pane on the
/// same evidence it uses for everything else — no private "have I
/// done this one" flag.
#[test]
fn a_dormant_pane_is_not_a_reclamation_candidate() {
    let e = IdleEvidence {
        quiescent: true,
        // `Dormant` is quiet but is not `AwaitingUser`.
        awaiting_user: false,
        held: Duration::from_secs(86_400),
        idle_for: Duration::from_secs(86_400),
        cpu_delta_ns: 0,
        since_sample: Duration::from_secs(300),
        user_here: false,
        work_in_flight: false,
        own_timer: false,
        helper_bound: false,
    };
    assert!(!should_hibernate(e, HOUR));
}

/// The clock that decides is the session's, not the terminal's.
///
/// claude prints `Checking for updates` into the corner every
/// thirty minutes.  Those few dozen bytes make the pane busy for
/// half a minute, which resets the terminal's quiet clock — and
/// with a thirty-minute threshold that is a race the update check
/// always wins.  Measured on this machine: panes idle for eleven
/// hours, `held_s=1767` at every reset (thirty-three seconds
/// short), 23 update checks in one pane's log, zero reclamations.
///
/// The transcript does not move for chrome, so it is what counts.
#[test]
fn chrome_that_keeps_the_terminal_busy_does_not_stop_reclamation() {
    let half_hour = Duration::from_secs(1800);
    // Exactly the observed shape: the session has done nothing for
    // eleven hours, but the pane went quiet again only moments ago
    // because the update check just fired.
    let e = IdleEvidence {
        idle_for: Duration::from_secs(11 * 3600),
        held: Duration::from_secs(1),
        ..idle_for(0)
    };
    assert!(
        should_hibernate(e, half_hour),
        "eleven idle hours is idle, whatever the terminal was doing"
    );

    // And the converse still holds: a session that has been
    // working is not reclaimed just because its pane is quiet
    // while it thinks.
    let e = IdleEvidence {
        idle_for: Duration::from_secs(60),
        held: Duration::from_secs(11 * 3600),
        ..idle_for(0)
    };
    assert!(!should_hibernate(e, half_hour));
}

/// The log has to be able to say why a candidate is still waiting.
/// "Nothing happened" reads the same whether the policy is patient
/// or broken, and only one of those is fine.
#[test]
fn a_waiting_candidate_can_say_what_it_is_waiting_on() {
    // Ready to go → nothing to explain.
    assert_eq!(blocking_reason(idle_for(7200), HOUR), None);

    // Still accruing idle time.
    let (cat, line) = blocking_reason(idle_for(1800), HOUR).expect("a reason");
    assert_eq!(cat, "below_threshold");
    assert!(line.contains("1800s of 3600s"), "got {line:?}");

    // The category must NOT move with the number, or "log on
    // change" logs on every scan — which is exactly what happened
    // in production: 3030 lines in 25 minutes.
    let (later, _) = blocking_reason(idle_for(1801), HOUR).expect("a reason");
    assert_eq!(later, cat, "the dedup key must be value-free");

    // Past the threshold, but the CPU window is too short to mean
    // anything yet.
    let e = IdleEvidence {
        since_sample: Duration::from_secs(5),
        ..idle_for(7200)
    };
    let (cat, line) = blocking_reason(e, HOUR).expect("a reason");
    assert_eq!(cat, "short_cpu_sample");
    assert!(line.contains("spans only 5s"), "got {line:?}");

    // Past the threshold, window fine, but something is running.
    let e = IdleEvidence {
        cpu_delta_ns: 2_500_000_000,
        ..idle_for(7200)
    };
    let (cat, line) = blocking_reason(e, HOUR).expect("a reason");
    assert_eq!(cat, "cpu_busy");
    assert!(line.contains("2500ms of cpu"), "got {line:?}");

    // Not a candidate at all → silence, not noise.  Every busy pane
    // in the window would otherwise explain itself once a second.
    let e = IdleEvidence { awaiting_user: false, ..idle_for(7200) };
    assert_eq!(blocking_reason(e, HOUR), None);
    let e = IdleEvidence { quiescent: false, ..idle_for(7200) };
    assert_eq!(blocking_reason(e, HOUR), None);
}

/// A session with a process of its own running is not resting,
/// however quiet it looks.  Measured on a live pane:
/// `zsh → cargo-fuzz + tail`, all at 0.0 % CPU and no transcript
/// activity — the clock and the CPU gate both say "idle", and
/// reclaiming would kill the build.
#[test]
fn a_session_running_something_of_its_own_is_never_reclaimed() {
    let e = IdleEvidence { work_in_flight: true, ..idle_for(86_400) };
    assert!(!should_hibernate(e, HALF_HOUR));
    let (cat, _) = blocking_reason(e, HALF_HOUR).expect("a reason");
    assert_eq!(cat, "work_in_flight");
}

/// A `/loop` autorun waits on a timer inside claude: no processes,
/// no transcript, and a pane that looks exactly like one waiting
/// for a person.  Reclaiming it ends the run.
#[test]
fn a_session_waiting_on_its_own_timer_is_never_reclaimed() {
    let e = IdleEvidence { own_timer: true, ..idle_for(86_400) };
    assert!(!should_hibernate(e, HALF_HOUR));
    let (cat, _) = blocking_reason(e, HALF_HOUR).expect("a reason");
    assert_eq!(cat, "own_timer");
}

/// The traces those two leave, as they appear in a transcript and
/// a process table.
#[test]
fn the_two_vetoes_recognise_what_they_are_looking_for() {
    assert!(tail_mentions_own_timer(
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"ScheduleWakeup"}]}}"#
    ));
    assert!(tail_mentions_own_timer("… <<autonomous-loop-dynamic>> …"));
    assert!(!tail_mentions_own_timer(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
    ));

    let row = |pid: i32, ppid: i32, comm: &str| pidtree::ProcRow {
        pid,
        ppid,
        comm: comm.into(),
        start_unix: 0,
        start_us: 0,
        pgid: pid,
        tty_dev: 1,
        tty_fg_pgid: 1,
        status: libc::SSLEEP,
    };
    let claude = 100;
    // A resting session: an MCP server, a language server with its
    // own helper, caffeinate, and an empty shell.
    let resting = vec![
        row(claude, 1, "claude"),
        row(101, claude, "smix-mcp"),
        row(102, claude, "rust-analyzer"),
        row(103, 102, "rust-analyzer-proc-macro-srv"),
        row(104, claude, "caffeinate"),
        row(105, claude, "zsh"),
        row(108, claude, "sourcekit-lsp"),
        row(109, claude, "clangd"),
    ];
    assert!(work_in_flight(claude, &pidtree::ProcTable::from_rows(resting.clone())).is_empty(), "none of that is work");

    // The same session with a job under its shell.
    let mut working = resting.clone();
    working.push(row(106, 105, "cargo-fuzz"));
    assert!(!work_in_flight(claude, &pidtree::ProcTable::from_rows(working)).is_empty(), "a job under the shell is work");

    // An unrecognised helper counts as work: the allowlist fails
    // towards not reclaiming, never towards killing something.
    let mut unknown = resting.clone();
    unknown.push(row(107, claude, "some-new-helper"));
    assert!(!work_in_flight(claude, &pidtree::ProcTable::from_rows(unknown)).is_empty());
}

#[test]
fn hibernates_a_pane_that_has_been_quiet_past_the_threshold() {
    assert!(should_hibernate(idle_for(3600), HOUR));
    assert!(should_hibernate(idle_for(7200), HOUR));
}

#[test]
fn refuses_below_the_threshold() {
    assert!(!should_hibernate(idle_for(3599), HOUR));
    assert!(!should_hibernate(idle_for(0), HOUR));
}

/// The state machine's own verdict is a veto: without confirmed
/// quiescence nothing else matters.
#[test]
fn refuses_when_the_state_machine_is_not_confident() {
    let e = IdleEvidence { quiescent: false, ..idle_for(7200) };
    assert!(!should_hibernate(e, HOUR));
}

/// `Empty` is quiet too — an empty pane with no claude in it.
/// There is nothing to reclaim, and acting on it would mean
/// killing whatever the user is about to start.
#[test]
fn refuses_a_quiet_pane_that_has_no_claude() {
    let e = IdleEvidence { awaiting_user: false, ..idle_for(7200) };
    assert!(!should_hibernate(e, HOUR));
}

/// A silent transcript with a busy subtree is the case a
/// transcript-only policy would get wrong: a background task, or a
/// tool that hasn't written its result yet.
#[test]
fn refuses_when_the_subtree_is_burning_cpu() {
    let e = IdleEvidence {
        cpu_delta_ns: IDLE_CPU_TOLERANCE_NS + 1,
        ..idle_for(7200)
    };
    assert!(!should_hibernate(e, HOUR));
    // …but an idling MCP server's tick is under the tolerance and
    // must not block reclamation forever.
    let e = IdleEvidence {
        cpu_delta_ns: IDLE_CPU_TOLERANCE_NS / 2,
        ..idle_for(7200)
    };
    assert!(should_hibernate(e, HOUR));
}

/// A CPU delta measured across no time says nothing.  This is the
/// state right after a restart, when the first sample has just
/// been taken.
#[test]
fn refuses_on_a_cpu_sample_that_spans_no_time() {
    let e = IdleEvidence { since_sample: Duration::from_secs(1), ..idle_for(7200) };
    assert!(!should_hibernate(e, HOUR));
}

/// The resume line goes into a shell verbatim, so the profile
/// number has to produce a binary that exists — `claude255` was
/// what a naive format produced for "no profile".
/// The profile decides which config dir — which account — the
/// The reclamation types the session back under the profile it was
/// running.
///
/// `claudeN` is an interactive alias in the user's rc file
/// (`alias claude1='CLAUDE_CONFIG_DIR=~/.claude-profile-1 claude'`),
/// so reproducing the alias would depend on that file still defining
/// it; setting the variable is the same thing without the
/// dependency.  Read off the script itself — that is what runs.
#[test]
fn the_reclamation_resumes_under_the_profile_it_was_running() {
    let line = |dir: Option<&str>| -> String {
        let op = reclaim_op("abc-123", dir, Some(fid(1)), 2, None, &CliChoice::default()).expect("script builds");
        let bytes = op
            .steps
            .iter()
            .filter(|s| s.label == "resume")
            .find_map(|s| match &s.kind {
                pty_op::StepKind::Send(b) => Some(b.clone()),
                _ => None,
            })
            .expect("the script types something");
        String::from_utf8(bytes).unwrap()
    };
    // Asserted in pieces rather than as one string: the line now
    // carries a `--settings` document naming this binary by its
    // absolute path, which differs between a dev build, the
    // bundle, and `binaries/current`. Pinning the whole line would
    // go red on where it was built rather than on what it does.
    let with_dir = line(Some("/Users/x/.claude-profile-3"));
    assert!(
        with_dir.starts_with(
            "printf '\\033[H\\033[2J'; CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' claude "
        ),
        "{with_dir}"
    );
    assert!(with_dir.ends_with(" --resume abc-123\r"), "{with_dir}");

    // No dir observed = the default profile's own entry point.
    let no_dir = line(None);
    assert!(no_dir.starts_with("printf '\\033[H\\033[2J'; claude "), "{no_dir}");
    assert!(no_dir.ends_with(" --resume abc-123\r"), "{no_dir}");
}

/// The dir lands inside single quotes on a real command line, so a
/// A profile that cannot be quoted stops the whole reclamation.
///
/// The old code fell back to a plain `claude --resume`, which brings
/// the session back under a *different account* — worse than leaving
/// it running.  Now the command refuses to build and the script is
/// never created.
#[test]
fn a_config_dir_that_could_break_out_of_quoting_blocks_the_whole_op() {
    for bad in [
        "/tmp/a'; rm -rf ~; '",
        "/tmp/a\nclaude --dangerously",
        "/tmp/$(whoami)",
        "/tmp/`id`",
        "",
    ] {
        assert!(
            reclaim_op("u", Some(bad), Some(fid(1)), 2, None, &CliChoice::default()).is_none(),
            "{bad:?} should stop the reclamation"
        );
    }
    assert!(reclaim_op("u", Some("/home/u/.claude-profile-1"), Some(fid(1)), 2, None, &CliChoice::default()).is_some());
}

/// A session whose profile could not be read must not be
/// reclaimed at all: there is no resume line that is known to put
/// it back where it was, and guessing means a different account.
#[test]
fn an_unreadable_profile_blocks_reclamation() {
    let sandbox = sandbox_state_dir();
    let dir = sandbox.path().to_path_buf();
    let host = FakeHost::new(dir.clone());
    host.set_status(7, idle_view(Duration::from_secs(7200)));
    let mut plugin = ClaudecodePlugin::new();
    plugin.shelld = Some(Arc::new(ShelldClient::new(None)));

    let claude_like = std::process::id() as i32;
    let mut scan = scan_with(7, claude_like, "uuid-1");
    scan.new_meta.get_mut(&7).unwrap().profile_num = PROFILE_UNKNOWN;
    scan.new_cpu.insert(7, (1_000, SystemTime::now() - Duration::from_secs(120)));
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);
    let mut scan = scan_with(7, claude_like, "uuid-1");
    scan.new_meta.get_mut(&7).unwrap().profile_num = PROFILE_UNKNOWN;
    plugin.run_idle_policy_at(&host, &scan, TEST_THRESHOLD);

    assert!(
        host.begun.lock().unwrap().is_empty(),
        "an unnamed profile must not be reclaimed"
    );
    assert!(plugin.dormant.is_empty());
}

#[test]
fn hibernate_can_be_turned_off_and_tuned() {
    // Pin the settings this test reasons about.
    //
    // `hibernate_after` reads the settings file when the env knob
    // is unset, and with nothing pinned that is the **developer's
    // own** `~/Library/Caches/marspot/settings.toml`.  This test
    // passed until the day its author turned reclamation off in
    // the panel, and then failed on their machine and nobody
    // else's — a test that reports the state of the machine it
    // runs on rather than the state of the code.
    marspot::settings::set_for_test(marspot::settings::Settings::default());
    let key = "MARSPOT_CC_IDLE_HIBERNATE_S";
    let mut env = EnvGuard::new();
    env.set(key, "0");
    assert_eq!(hibernate_after(), None, "0 disables reclamation");
    env.set(key, "90");
    assert_eq!(hibernate_after(), Some(Duration::from_secs(90)));
    env.set(key, "nonsense");
    assert_eq!(
        hibernate_after(),
        Some(HALF_HOUR),
        "an unparsable value falls back to the default rather than to off"
    );
    env.remove(key);
    assert_eq!(hibernate_after(), Some(HALF_HOUR));
}

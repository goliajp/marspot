//! The script a profile switch runs: what it stops, what it types, and when it lets go.

use super::*;

/// With an idle background job holding the session, the cycle stops
/// that job after the pane's claude and before the resume is typed —
/// `claude --resume` checks for a live holder the moment it starts.
#[test]
fn a_cycle_through_a_background_hold_stops_it_before_resuming() {
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), Some(fid(5151)), 4200, None, &known()).expect("cycle");
    let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
    let stop = labels.iter().position(|l| *l == "stop_background").expect("stop step present");
    let resume = labels.iter().position(|l| *l == "resume").expect("resume step present");
    assert!(stop < resume, "{labels:?}");
    assert!(matches!(
        op.steps[stop].kind,
        pty_op::StepKind::Terminate { target: pidtree::ProcId { pid: 5151, .. }, .. }
    ));
    // No holder: no extra step.
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &known()).unwrap();
    assert!(!op.steps.iter().any(|s| s.label == "stop_background"));
    // A holder that is the pane's own claude is not stopped twice.
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), Some(fid(4242)), 4200, None, &known()).unwrap();
    assert!(!op.steps.iter().any(|s| s.label == "stop_background"));
}

/// The switch says the thing the person used to have to type, and
/// says it without touching the composer.
///
/// `--resume` reopens the session and waits — the refused turn is
/// not retried by itself, which is why every switch so far has
/// ended with someone typing "继续" by hand.  It used to be typed
/// by us instead: pasted into the composer, then a return.  That
/// path is wrong however well it works, because the composer is
/// the person's buffer and every failure leaves our sentence
/// sitting in it unsent — seen three times on 2026-10-01.
///
/// `claude [options] [prompt]` takes the sentence as an argument
/// and submits it itself, so it rides in on the one line that
/// starts the new process.  What this test pins is that there is
/// no second path: nothing in the run types the sentence.
#[test]
fn a_cycle_says_continue_on_the_command_line() {
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &known()).expect("cycle");
    let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
    let resume = labels.iter().position(|l| *l == "resume").expect("resume");
    let line = match &op.steps[resume].kind {
        pty_op::StepKind::Send(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => panic!("the resume step writes a command line, not {}", op.steps[resume].label),
    };
    assert!(
        line.contains(&resume_word()),
        "the sentence is an argument of the command that starts claude: {line}"
    );
    // A flag and its value are adjacent or they are not a flag and
    // its value. This is what keeping quoted arguments in a list of
    // their own once broke -- the quoted ones were appended last,
    // which put the sentence between `--resume` and the id it
    // names. There is one bare flag and one quoted positional on
    // this line, so the line itself is the case.
    assert!(
        line.contains("--resume u-1"),
        "the id follows its flag: {line}"
    );
    assert!(
        line.find("--resume").unwrap() < line.find(&resume_word()).unwrap(),
        "and the sentence comes after the pair, not inside it: {line}"
    );
    assert!(
        !op.steps
            .iter()
            .any(|s| matches!(&s.kind, pty_op::StepKind::Submit { .. })),
        "nothing in the run submits anything into the pane: {labels:?}"
    );
    assert!(
        !labels.contains(&"enter"),
        "and nothing presses return in it either: {labels:?}"
    );
}

/// A switch brings the session back on the model it was on.
///
/// Without the flag the new claude takes the new profile's default:
/// on 2026-10-03 a pane switched to Opus came back on Fable, on an
/// account whose Fable was spent.
#[test]
fn a_cycle_keeps_the_model_and_effort_it_was_on() {
    let choice = CliChoice {
        model: Some("claude-fable-5-1[1m]".into()),
        effort: Some("medium".into()),
    };
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &choice).expect("cycle");
    let line = op
        .steps
        .iter()
        .find_map(|s| match &s.kind {
            pty_op::StepKind::Send(bytes) if s.label == "resume" => {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
            _ => None,
        })
        .expect("a resume line");
    assert!(line.contains("--model 'claude-fable-5-1[1m]'"), "quoted, or the shell globs it: {line}");
    assert!(line.contains("--effort medium"), "{line}");

    // Without a model there is no line at all: a claude started
    // without `--model` takes the shared settings file's default, which
    // any session's `/model` rewrites for every account.
    let unknown = CliChoice { model: None, effort: Some("medium".into()) };
    assert!(profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &unknown).is_none());
    assert!(reclaim_op("u-1", None, Some(fid(4242)), 4200, None, &unknown).is_none());
    // a fresh start (no transcript) names its model too
    let fresh = profile_cycle_op(Some(&resume_word()), "u-1", false, 4, fid(4242), None, 4200, None, &known()).expect("cycle");
    let sent: String = fresh
        .steps
        .iter()
        .filter_map(|s| match &s.kind {
            pty_op::StepKind::Send(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .collect();
    assert!(!sent.contains("--resume") && sent.contains("--model 'claude-opus-5-5'"), "{sent}");
}

/// What an automatic switch needs to know about the pane first.
#[test]
fn a_switch_needs_the_panes_model_confirmed_twice() {
    let carry = |id: &str| CliChoice { model: Some(id.into()), effort: Some("medium".into()) };
    assert_eq!(confirm_choice(Some("fable-5-1"), &carry("claude-fable-5-1[1m]")), Ok(()));
    assert_eq!(confirm_choice(Some("opus-5-5"), &carry("claude-opus-5-5")), Ok(()));
    assert!(confirm_choice(None, &carry("claude-opus-5-5")).is_err(), "model not known");
    assert!(
        confirm_choice(Some("opus-5-5"), &CliChoice::default()).is_err(),
        "nothing to start the new claude on"
    );
    assert!(
        confirm_choice(Some("opus-5-5"), &carry("claude-fable-5-1[1m]")).is_err(),
        "the two reads disagree"
    );
    let no_effort = CliChoice { model: Some("claude-opus-5-5".into()), effort: None };
    assert!(confirm_choice(Some("opus-5-5"), &no_effort).is_err(), "effort not known");
}

/// The hook's record is a file: what goes on a command line from it
/// is checked first.
#[test]
fn the_hooks_record_names_the_model_as_claude_takes_it() {
    let _sandbox = sandbox_state_dir();
    let push = model_push_dir();
    fs::create_dir_all(&push).unwrap();
    let write = |uuid: &str, body: &str| fs::write(push.join(uuid), body).unwrap();

    write("u-new", "opus-5-5\n/t/u-new.jsonl\nmedium\npids=1,2\nid=claude-opus-5-5\n");
    assert_eq!(
        pushed_choice("u-new"),
        CliChoice { model: Some("claude-opus-5-5".into()), effort: Some("medium".into()) }
    );
    write("u-old", "opus-5-5\n/t/u-old.jsonl\nhigh\npids=1,2\n");
    assert_eq!(
        pushed_choice("u-old"),
        CliChoice { model: None, effort: Some("high".into()) },
        "a record from before the id line passes no model"
    );
    write("u-bad", "opus-5-5\n/t/u-bad.jsonl\nloud\npids=1,2\nid=opus; rm -rf ~\n");
    assert_eq!(pushed_choice("u-bad"), CliChoice::default(), "neither value is one claude takes");
    assert_eq!(pushed_choice("../u-new"), CliChoice::default(), "a uuid is not a path");
}

/// Starting a claude does not install a hook that runs on the
/// person's keystrokes.
///
/// Every pane marspot started carried a `UserPromptSubmit` hook so
/// that text typed into it could be confirmed rather than read off
/// the screen. Nothing reads a receipt any more -- the sentence
/// moved to the command line and no production path submits -- and
/// the hook was a fork and exec of marspot-shell on every prompt
/// the person sent by hand: median 7.6 ms, up to 25.9 ms, measured.
#[test]
fn a_resumed_pane_gets_no_hook_on_the_persons_keystrokes() {
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &known()).expect("cycle");
    let line = match &op.steps.iter().find(|s| s.label == "resume").unwrap().kind {
        pty_op::StepKind::Send(b) => String::from_utf8_lossy(b).into_owned(),
        _ => unreachable!(),
    };
    assert!(
        !line.contains("UserPromptSubmit"),
        "no hook rides in on the line that starts claude: {line}"
    );
    assert!(
        !line.contains("--settings"),
        "and nothing is added to the person's own settings: {line}"
    );
}

/// A switch must not move the session it resumes.
///
/// `claude --resume` resolves the id against the project of the
/// directory it runs in, and binds what it reopens to that
/// directory. The pane's shell is not reliably standing where the
/// session lives, and when it is not, the switch relocates the
/// conversation: a torajs session came back as `devops` and loaded
/// devops's rules, with nothing to show for it but a changed pane
/// title.
#[test]
fn a_cycle_resumes_the_session_where_it_lives() {
    let op = profile_cycle_op(
        Some(&resume_word()),
        "u-1",
        true,
        4,
        fid(4242),
        None,
        4200,
        Some("/Users/x/workspace/goliajp/torajs"),
        &known(),
    )
    .expect("cycle");
    let resume = op
        .steps
        .iter()
        .find(|s| s.label == "resume")
        .expect("resume step");
    let line = match &resume.kind {
        pty_op::StepKind::Send(b) => String::from_utf8_lossy(b).into_owned(),
        _ => panic!("the resume step writes a command line"),
    };
    assert!(
        line.contains("cd '/Users/x/workspace/goliajp/torajs' && "),
        "it goes to the session's own directory first: {line}"
    );
    // Order matters and is not cosmetic: `exec FOO=bar cmd` runs a
    // program named `FOO=bar`, which is how every switch came to
    // exit 127 and then wait twenty seconds for a claude that had
    // never been started.
    let exec_at = line.find("exec ").expect("the subshell execs the program");
    let cd_at = line.find("cd '").expect("after the cd");
    assert!(cd_at < exec_at, "{line}");
    if let Some(env_at) = line.find("CLAUDE_CONFIG_DIR=") {
        assert!(
            env_at < exec_at,
            "the environment is set before exec, not after it: {line}"
        );
    }
    assert!(
        line.trim_end().ends_with(')'),
        "and does it in a subshell, so the person's prompt is not moved: {line}"
    );

    // Without one, the line is what it has always been: run where
    // the shell stands. Not knowing where a session lives is not a
    // reason to refuse to move it off a spent account.
    let plain = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &known()).expect("cycle");
    let line = match &plain.steps.iter().find(|s| s.label == "resume").unwrap().kind {
        pty_op::StepKind::Send(b) => String::from_utf8_lossy(b).into_owned(),
        _ => unreachable!(),
    };
    assert!(!line.contains("cd "), "no directory, no cd: {line}");
}

/// A directory that could break out of its quoting is refused, and
/// refusing it costs the whole line rather than producing one that
/// runs somewhere else.
#[test]
fn a_directory_that_cannot_be_quoted_is_not_guessed_at() {
    assert!(
        !profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, Some("/tmp/it's here"), &known())
            .map(|op| op.steps.iter().any(|s| s.label == "resume"))
            .unwrap_or(false),
        "a path with a quote in it produces no resume line at all"
    );
}

/// The hold over the pane lifts when the run ends, and what is
/// under it is a screen we cleared ourselves -- so the run has to
/// outlast the new claude's first frame.
///
/// This was here by accident until the sentence moved onto the
/// command line: the submit step waited for the composer to redraw
/// with the pasted text in it, which is a first frame by another
/// name. Taking the typing away took the wait with it and the pane
/// went black for as long as the new process took to draw.
#[test]
fn a_cycle_outlasts_the_first_frame_of_the_new_claude() {
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &known()).expect("cycle");
    let labels: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
    let at = |n: &str| labels.iter().position(|l| *l == n);
    let ready = at("await_tui_ready").expect("the terminal is asked first");
    let frame = at("first_frame").expect("and then waited for");
    let restore = at("restore_composer").expect("the composer comes back last");
    assert!(ready < frame && frame < restore, "{labels:?}");
    assert!(
        matches!(
            &op.steps[frame].kind,
            pty_op::StepKind::AwaitQuiet { min_bytes, .. } if *min_bytes > 0
        ),
        "the wait is for bytes drawn, not for a length of time: {labels:?}"
    );
    assert!(
        !op.steps[frame].proceed_on_timeout,
        "a pane that never draws is not a pane to unfreeze onto: {labels:?}"
    );
    assert!(
        op.steps[ready].proceed_on_timeout,
        "asking the terminal stays a deadline, not a precondition: {labels:?}"
    );
}

/// The op that answers a refused click must not take anything from
/// the pane: a click the cycle declines has to cost no more than a
/// click that did nothing at all.
#[test]
fn cycle_blocked_op_takes_nothing_from_the_pane() {
    let op = cycle_blocked_op("⚠ held by bg job");
    assert!(!op.lock_keys, "a refused click must not eat the keyboard");
    assert!(!op.hold_screen, "nothing to hide — the pane is untouched");
    assert_eq!(op.badge.as_deref(), Some("⚠ held by bg job"));
}

/// Everything a `PtyOp` types into the pane, joined.
fn sent_text(op: &pty_op::PtyOp) -> String {
    op.steps
        .iter()
        .filter_map(|s| match &s.kind {
            pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A session the hook has named but claude has not written a
/// transcript for: nothing to resume, so the cycle starts claude
/// plain rather than refusing.
///
/// Reported 2026-09-16 — entering cc and clicking the badge did
/// nothing, twice, and kept doing nothing until the session had
/// answered something.
#[test]
fn a_session_with_no_turn_yet_cycles_by_starting_fresh() {
    let op = profile_cycle_op(Some(&resume_word()), "u-1", false, 3, fid(4242), None, 4200, None, &known())
        .expect("a named session with no transcript can still cycle");
    let sent = sent_text(&op);
    assert!(
        sent.contains(".claude-profile-3"),
        "must still switch profile: {sent:?}"
    );
    assert!(
        !sent.contains("--resume"),
        "nothing to resume, and --resume on a uuid claude never wrote fails: {sent:?}"
    );

    // The counter-case: once the transcript exists, the session
    // travels with the switch.
    let op = profile_cycle_op(Some(&resume_word()), "u-1", true, 3, fid(4242), None, 4200, None, &known()).expect("normal path");
    let sent = sent_text(&op);
    assert!(sent.contains("--resume"), "{sent:?}");
    assert!(sent.contains("u-1"), "{sent:?}");
}

/// The line an account switch types, and the one a reclaim types, both
/// give claude marspot's MCP config, before `--resume` so the flag's
/// list of files ends there; the sentence comes after `--` so it is
/// not read as one more file.
#[test]
fn a_switch_or_reclaim_starts_claude_with_the_mcp_config() {
    let state = TempDir::new("cycle-mcp");
    let mut env = EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", state.path()).set("MARSPOT_PEER_BIN", "/a/marspot-peer");
    let typed = |op: pty_op::PtyOp| {
        op.steps
            .iter()
            .find(|s| s.label == "resume")
            .and_then(|s| match &s.kind {
                pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .expect("the resume step writes a command line")
    };
    let flag = format!(" --mcp-config '{}' --allowedTools 'mcp__marspot' --resume u-1 ", state.join("claude-mcp.json").display());
    let cycle = typed(
        profile_cycle_op(Some(&resume_word()), "u-1", true, 4, fid(4242), None, 4200, None, &known()).unwrap(),
    );
    assert!(cycle.contains(&flag), "{cycle}");
    assert!(cycle.ends_with(&format!(" -- '{}'\r", resume_word())), "{cycle}");
    let reclaim = typed(reclaim_op("u-1", None, Some(fid(1)), 2, None, &known()).unwrap());
    assert!(reclaim.contains(flag.trim_end()), "{reclaim}");
    assert!(state.join("claude-mcp.json").is_file());
}

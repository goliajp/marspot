use super::*;

fn switched() -> transcript::Switch {
    transcript::Switch { at: "11/10 18:09".into(), left: "has exited".into(), stopped: true }
}
use crate::plugins::handoff::history::tmp;

/// Leaving kimi, and coming back to the session the pane had there.
#[test]
fn a_kimi_pane_hands_over_its_session_and_is_resumed_in_the_same_home() {
    use kimi::fixture::{session, ID, STOP_HOOK};
    let home = tmp("switch-kimi");
    session(&home, ID, STOP_HOOK, None);
    let leaving = Leaving {
        agent: Agent::Kimi,
        proc: marspot::pidtree::ProcId { pid: 1, start_us: 1 },
        shell_pid: 1,
        home: home.clone(),
        session_id: Some(ID.into()),
        cwd: Some("/w/a".into()),
        go_on: false,
    };
    let dir = tmp("switch-kimi-ledger");
    let o = prepare_in(7, &dir, &leaving, Agent::Claude, None, &Ledger::default(), history_of(&leaving), &switched()).unwrap();
    assert!(o.message.expect("kimi's turn is handed over").contains("Kimi Code"));
    let doc = std::fs::read_to_string(ledger::doc_path(&dir, 7)).unwrap();
    assert!(doc.contains("say hi") && doc.contains("mock reply done") && !doc.contains("hook_result"), "{doc}");
    assert_eq!(o.next.kimi, Some(Side { id: ID.into(), offset: STOP_HOOK.len() as u64 }));
    assert_eq!(resumable(&o.next, Agent::Kimi, &home, None).as_deref(), Some(ID));
    assert_eq!(resumable(&o.next, Agent::Kimi, &tmp("switch-kimi-other"), None), None, "another account's home does not hold it");
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    let line = s(command(Agent::Kimi, Path::new("/Users/x/.kimi-code-profile-2"), Some(ID), None).unwrap());
    assert!(line.contains("KIMI_CODE_HOME='/Users/x/.kimi-code-profile-2'") && line.ends_with(&format!(" kimi --session {ID}\r")), "{line}");
    assert_eq!(target_name(Agent::Kimi, 0), "kimi");
}

/// A kimi pane told its account was nearly out, moved to another
/// kimi account: the new session is asked to go on, and one that
/// was not told is asked to wait, as before.
#[test]
fn a_told_kimi_pane_is_asked_to_go_on_on_the_new_account() {
    use kimi::fixture::{session, ID, STOP_HOOK};
    let home = tmp("switch-kimi-told");
    session(&home, ID, STOP_HOOK, None);
    let leaving = |go_on| Leaving {
        agent: Agent::Kimi,
        proc: marspot::pidtree::ProcId { pid: 1, start_us: 1 },
        shell_pid: 1,
        home: home.clone(),
        session_id: Some(ID.into()),
        cwd: Some("/w/a".into()),
        go_on,
    };
    let said = |go_on| {
        let l = leaving(go_on);
        let dir = tmp("switch-kimi-told-ledger");
        prepare_in(7, &dir, &l, Agent::Kimi, None, &Ledger::default(), history_of(&l), &switched())
            .unwrap()
            .message
            .expect("kimi's turn is handed over")
    };
    let told = said(true);
    assert!(told.contains("go on with the task") && !told.contains("Do not run commands"), "{told}");
    let untold = said(false);
    assert!(untold.contains("wait for my next instruction") && untold.contains("Do not run commands"), "{untold}");
}

/// A claude started by a handoff gets marspot's MCP config, ahead
/// of `--resume`; the other agents' lines do not.
#[test]
fn a_switch_to_claude_starts_it_with_the_mcp_config() {
    let state = marspot_term::test_env::TempDir::new("switch-mcp");
    let mut env = marspot_term::test_env::EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", state.path()).set("MARSPOT_PEER_BIN", "/a/marspot-peer");
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    let mcp = format!("--mcp-config '{}' --allowedTools 'mcp__marspot'", state.join("claude-mcp.json").display());
    let claude = s(command(Agent::Claude, Path::new("/Users/x/.claude-profile-3"), Some("u-1"), None).unwrap());
    assert!(claude.ends_with(&format!(" claude {mcp} --resume u-1 --model opus\r")), "{claude}");
    let fresh = s(command(Agent::Claude, Path::new("/Users/x/.claude-profile-3"), None, None).unwrap());
    assert!(fresh.ends_with(&format!(" claude {mcp} --model opus\r")), "{fresh}");
    assert!(state.join("claude-mcp.json").is_file());
    let codex = s(command(Agent::Codex, Path::new("/Users/x/.codex-profile-2"), None, None).unwrap());
    assert!(!codex.contains("--mcp-config"), "{codex}");
}

#[test]
fn the_command_resumes_by_id_or_starts_fresh() {
    let _state = marspot_term::test_env::sandbox_state_dir();
    let h = Path::new("/Users/x/.codex-profile-2");
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    assert!(s(command(Agent::Codex, h, Some("t-1"), None).unwrap()).contains("CODEX_HOME='/Users/x/.codex-profile-2' codex resume t-1\r"));
    assert!(s(command(Agent::Codex, h, None, None).unwrap()).ends_with("codex\r"));
    let c = Path::new("/Users/x/.claude-profile-3");
    let claude = s(command(Agent::Claude, c, Some("u-1"), None).unwrap());
    assert!(
        claude.contains("CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' claude "),
        "{claude}"
    );
    assert!(claude.ends_with(" --resume u-1 --model opus\r"), "{claude}");
    // Neither agent is handed anything that runs on the person's
    // keystrokes. A Claude pane used to carry a `UserPromptSubmit`
    // hook so its submits could be confirmed; nothing reads a
    // receipt now, and the hook forked marspot on every prompt
    // they sent by hand.
    for line in [&claude, &s(command(Agent::Codex, h, None, None).unwrap())] {
        assert!(!line.contains("--settings"), "{line}");
        assert!(!line.contains("UserPromptSubmit"), "{line}");
    }
}

fn claude_history(dir: &Path, uuid: &str, body: &str) -> PathBuf {
    let p = dir.join("projects/-w").join(format!("{uuid}.jsonl"));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
    p
}
fn ask(uuid: &str, parent: Option<&str>, text: &str) -> String {
    let p = parent.map(|p| format!("\"{p}\"")).unwrap_or("null".into());
    format!(r#"{{"uuid":"{uuid}","parentUuid":{p},"type":"user","message":{{"content":"{text}"}}}}"#) + "\n"
}

fn leaving_claude(home: &Path, uuid: &str) -> Leaving {
    Leaving {
        agent: Agent::Claude,
        proc: marspot::pidtree::ProcId { pid: 1, start_us: 0 },
        shell_pid: 1,
        home: home.to_path_buf(),
        session_id: Some(uuid.into()),
        cwd: None,
        go_on: false,
    }
}

/// There and back: the second handoff out of the same session
/// carries only what is new, and the pane still has one document.
#[test]
fn a_second_handoff_carries_only_what_is_new() {
    let d = tmp("switch");
    let state = d.join("handoff");
    std::fs::create_dir_all(&state).unwrap();
    let first = ask("a", None, "first ask");
    let p = claude_history(&d, "u-1", &first);
    let l = leaving_claude(&d, "u-1");

    let o = prepare_in(9, &state, &l, Agent::Codex, None, &Ledger::default(), Some(p.clone()), &switched()).unwrap();
    let msg = o.message.expect("something to hand over");
    assert!(msg.starts_with(transcript::MARKER) && msg.contains("9.md"));
    let doc = std::fs::read_to_string(ledger::doc_path(&state, 9)).unwrap();
    assert!(doc.contains("first ask"));
    assert_eq!(o.next.claude, Some(Side { id: "u-1".into(), offset: first.len() as u64 }));

    // Back in claude later: the handoff turn from codex, then a new ask.
    let more = format!("{first}{}{}", ask("b", Some("a"), "[marspot handoff] from codex"), ask("c", Some("b"), "second ask"));
    std::fs::write(&p, &more).unwrap();
    let o2 = prepare_in(9, &state, &l, Agent::Codex, Some("t-1"), &o.next, Some(p), &switched()).unwrap();
    assert!(o2.message.unwrap().contains("since you left"));
    let doc = std::fs::read_to_string(ledger::doc_path(&state, 9)).unwrap();
    assert!(doc.contains("second ask"));
    assert!(!doc.contains("first ask"), "already handed over");
    assert!(!doc.contains("from codex"), "the other side's own content is not handed back");
    let files: Vec<_> = std::fs::read_dir(&state).unwrap().flatten().collect();
    assert_eq!(files.len(), 1, "one document per pane, overwritten");
}

/// Nothing new since last time: no message, so nothing is typed.
#[test]
fn nothing_new_means_nothing_is_typed() {
    let d = tmp("switch-empty");
    let body = ask("a", None, "only ask");
    let p = claude_history(&d, "u-1", &body);
    let mut led = Ledger::default();
    led.set(Agent::Claude, Some(Side { id: "u-1".into(), offset: body.len() as u64 }));
    let o = prepare_in(1, &d, &leaving_claude(&d, "u-1"), Agent::Codex, None, &led, Some(p), &switched()).unwrap();
    assert!(o.message.is_none());
    let o = prepare_in(1, &d, &leaving_claude(&d, "u-9"), Agent::Codex, None, &led, None, &switched()).unwrap();
    assert!(o.message.is_none(), "an agent with no history hands nothing over");
}

/// Going fresh to an agent forgets the session the pane had there;
/// resuming keeps it.
#[test]
fn a_fresh_start_forgets_the_old_session() {
    let d = tmp("switch-forget");
    let mut led = Ledger::default();
    led.set(Agent::Codex, Some(Side { id: "t-old".into(), offset: 5 }));
    let o = prepare_in(1, &d, &leaving_claude(&d, "u"), Agent::Codex, None, &led, None, &switched()).unwrap();
    assert_eq!(o.next.codex, None);
    let o = prepare_in(1, &d, &leaving_claude(&d, "u"), Agent::Codex, Some("t-old"), &led, None, &switched()).unwrap();
    assert_eq!(o.next.codex, led.codex);
}

/// Leaving a codex pane whose argv names no session: its rollout is
/// the newest one whose opening `session_meta` names the pane's
/// directory, and what it holds is handed over. A file that does not
/// open with that record is not a rollout codex wrote and is never
/// taken, whatever directory its later records name.
#[test]
fn a_codex_pane_without_a_session_id_is_found_by_its_session_meta() {
    let user = |t: &str| {
        format!(r#"{{"type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"UserMessage","content":[{{"type":"text","text":"{t}"}}]}}}}}}"#) + "\n"
    };
    let turn = r#"{"type":"turn_context","payload":{"cwd":"/w/a","model":"m"}}"#.to_string() + "\n";
    let meta = r#"{"type":"session_meta","payload":{"id":"t-1","cwd":"/w/a"}}"#.to_string() + "\n";
    let leaving = |home: &Path| Leaving {
        agent: Agent::Codex,
        proc: marspot::pidtree::ProcId { pid: 1, start_us: 0 },
        shell_pid: 1,
        home: home.to_path_buf(),
        session_id: None,
        cwd: Some("/w/a".into()),
        go_on: false,
    };

    let home = tmp("switch-codex-meta");
    let day = home.join("sessions/2026/09/22");
    std::fs::create_dir_all(&day).unwrap();
    let headless = day.join("rollout-2026-09-22T00-00-00-00000000-0000-4000-8000-000000000002.jsonl");
    std::fs::write(&headless, format!("{turn}{}", user("not codex's"))).unwrap();
    assert_eq!(history_of(&leaving(&home)), None, "no session_meta, no rollout");

    let real = day.join("rollout-2026-09-22T00-00-00-00000000-0000-4000-8000-000000000001.jsonl");
    std::fs::write(&real, format!("{meta}{turn}{}", user("second ask"))).unwrap();
    let found = history_of(&leaving(&home));
    assert_eq!(found.as_deref(), Some(real.as_path()));

    let state = home.join("handoff");
    std::fs::create_dir_all(&state).unwrap();
    let o = prepare_in(4, &state, &leaving(&home), Agent::Claude, Some("u-1"), &Ledger::default(), found, &switched()).unwrap();
    assert!(o.message.expect("codex's turn is handed over").contains("since you left"));
    assert!(std::fs::read_to_string(ledger::doc_path(&state, 4)).unwrap().contains("second ask"));
    assert!(o.next.codex.is_some(), "the codex side is recorded for next time");
}

#[test]
fn the_gemini_command_sets_its_home_and_resumes_by_id() {
    let h = Path::new("/Users/x/.gemini-profile-2");
    let id = "11111111-2222-4333-8444-555555555555";
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    assert_eq!(
        s(command(Agent::Gemini, h, Some(id), None).unwrap()),
        format!("printf '\\033[H\\033[2J'; GEMINI_CLI_HOME='/Users/x/.gemini-profile-2' gemini --resume {id}\r")
    );
    assert!(s(command(Agent::Gemini, h, None, None).unwrap()).ends_with("GEMINI_CLI_HOME='/Users/x/.gemini-profile-2' gemini\r"));
    // back to a session the pane left: on the model it was set to then
    assert!(
        s(command(Agent::Gemini, h, Some(id), Some("auto")).unwrap())
            .ends_with(&format!("GEMINI_CLI_HOME='/Users/x/.gemini-profile-2' gemini -m auto --resume {id}\r"))
    );
    assert!(command(Agent::Gemini, Path::new("/Users/x/it's"), None, None).is_none());
    assert_eq!(target_name(Agent::Gemini, 0), "gemini", "the default home has no number to show");
    assert_eq!(target_name(Agent::Gemini, 2), "gemini P2");
    assert_eq!(target_name(Agent::Codex, 1), "codex P1");
}

/// Leaving gemini: the session the plugin bound is found under the
/// home's config directory by project and id, and read into the
/// same document the other two agents produce.
#[test]
fn a_gemini_pane_hands_over_the_session_it_was_bound_to() {
    use gemini::fixture::{context, file_name, head, project, reply, user};
    let id = "11111111-2222-4333-8444-555555555555";
    let home = tmp("switch-gemini");
    let chats = project(&gemini::config_dir(&home), "/w/a", "a");
    let body = [head(id), context(), user("u1", "port the parser"), reply("g1", "Ported; tests pass.", "gemini-3-pro")].concat();
    let file = chats.join(file_name(id));
    std::fs::write(&file, &body).unwrap();
    let leaving = |session: Option<&str>, cwd: &str| Leaving {
        agent: Agent::Gemini,
        proc: marspot::pidtree::ProcId { pid: 1, start_us: 0 },
        shell_pid: 1,
        home: home.clone(),
        session_id: session.map(str::to_string),
        cwd: Some(cwd.into()),
        go_on: false,
    };
    assert_eq!(history_of(&leaving(None, "/w/a")), None, "no session named, none guessed");
    assert_eq!(history_of(&leaving(Some(id), "/w/other")), None);
    let found = history_of(&leaving(Some(id), "/w/a"));
    assert_eq!(found.as_deref(), Some(file.as_path()));

    let state = home.join("handoff");
    std::fs::create_dir_all(&state).unwrap();
    let o = prepare_in(6, &state, &leaving(Some(id), "/w/a"), Agent::Claude, None, &Ledger::default(), found, &switched()).unwrap();
    let msg = o.message.expect("gemini's turn is handed over");
    assert!(msg.starts_with(transcript::MARKER) && msg.contains("Gemini CLI"), "{msg}");
    let doc = std::fs::read_to_string(ledger::doc_path(&state, 6)).unwrap();
    assert!(doc.contains("port the parser") && doc.contains("Ported; tests pass."), "{doc}");
    assert!(!doc.contains("session_context"), "gemini's own preamble is not the user talking");
    assert_eq!(o.next.gemini, Some(Side { id: id.into(), offset: body.len() as u64 }));

    // and back: the ledger's session is resumable from that project only
    assert_eq!(resumable(&o.next, Agent::Gemini, &home, Some("/w/a")).as_deref(), Some(id));
    assert_eq!(resumable(&o.next, Agent::Gemini, &home, Some("/w/other")), None);
    assert_eq!(resumable(&o.next, Agent::Gemini, &home, None), None);
}

#[test]
fn resume_needs_the_history_to_be_reachable() {
    let d = tmp("switch-resume");
    let mut led = Ledger::default();
    led.set(Agent::Claude, Some(Side { id: "u-1".into(), offset: 0 }));
    assert_eq!(resumable(&led, Agent::Claude, &d, None), None);
    claude_history(&d, "u-1", "");
    assert_eq!(resumable(&led, Agent::Claude, &d, None).as_deref(), Some("u-1"));
}

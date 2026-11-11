//! Moving a pane from one agent to another: the menu rows, and the
//! script that does it.
//!
//! The order is the design (RFC-009): the document is written BEFORE
//! anything is killed, on a thread, so a history that cannot be read
//! costs a failed switch and not the user's running agent; and the
//! ledger is committed only once the new agent is up, so a switch that
//! fails part-way hands the same turns over again next time instead of
//! losing them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::ledger::{self, Ledger, Side};
use super::transcript::{self, Agent};
use super::tags::tag_for;
use super::{carried, claude, codex as codex_history, gemini as gemini_history, history, kimi as kimi_history};
use crate::plugins::pty_op::{self, PtyCommand, PtyOp, Step};
use crate::plugins::{claudecode, codex, gemini, kimi, pidtree};

/// An agent's account profiles: number and directory.  gemini or kimi
/// with no numbered profile has its default home, as number 0.
pub fn profiles(a: Agent) -> Vec<(u8, PathBuf)> {
    match a {
        Agent::Claude => {
            let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
            claudecode::discover_profiles()
                .into_iter()
                .map(|n| (n, home.join(format!(".claude-profile-{n}"))))
                .collect()
        }
        Agent::Codex => codex::profile_dirs(),
        Agent::Gemini => gemini::handoff_homes(),
        Agent::Kimi => kimi::handoff_homes(),
    }
}

/// How a target reads in a menu row and on the badge: `codex P2`, or
/// the bare name for gemini's default home.
fn target_name(to: Agent, profile: u8) -> String {
    match (to, profile) {
        (Agent::Gemini | Agent::Kimi, 0) => to.key().to_string(),
        _ => format!("{} P{profile}", to.key()),
    }
}

/// The rows a pane running `from` offers: one per profile of each other
/// agent.
pub fn menu_rows(from: Agent) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
    let rows = |to: Agent| {
        profiles(to).into_iter().map(move |(n, _)| marspot::shell_proto::PaneBadgeMenuItem {
            tag: tag_for(to, n),
            label: format!("hand off to {}", target_name(to, n)),
        })
    };
    from.others().into_iter().flat_map(rows).collect()
}

/// The agent a pane is leaving.
#[derive(Debug, Clone)]
pub struct Leaving {
    pub agent: Agent,
    /// The process taken down: named by pid and start time, so the
    /// switch stops this one and never a process that took its pid.
    pub proc: marspot::pidtree::ProcId,
    pub shell_pid: i32,
    /// `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GEMINI_CLI_HOME` / `KIMI_CODE_HOME` it runs under.
    pub home: PathBuf,
    /// The session, when the plugin knows it (claude's binding does;
    /// codex's is found on the job thread — see [`history_of`]).
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// Where the leaving agent's history is.  May read file tails, so it
/// runs on the job thread, not in a hook.
fn history_of(l: &Leaving) -> Option<PathBuf> {
    match l.agent {
        Agent::Claude => history::claude_transcript(&l.home, l.session_id.as_deref()?),
        Agent::Codex => match l.session_id.as_deref() {
            Some(id) => history::codex_rollout(&l.home, id),
            None => codex::newest_rollout_for_cwd(&l.home, l.cwd.as_deref()?),
        },
        Agent::Gemini => gemini::session_by_id(&gemini::config_dir(&l.home), l.cwd.as_deref()?, l.session_id.as_deref()?),
        Agent::Kimi => kimi::wire_of(&l.home, l.session_id.as_deref()?),
    }
}

/// The session the pane already has with `to`, if it can be resumed
/// from `to_home`.  gemini keeps sessions per project, so one of its
/// sessions is resumable only from the directory the line is typed in.
fn resumable(ledger: &Ledger, to: Agent, to_home: &Path, shell_cwd: Option<&str>) -> Option<String> {
    let side = ledger.side(to)?;
    let found = match to {
        Agent::Claude => history::claude_transcript(to_home, &side.id),
        Agent::Codex => history::codex_rollout(to_home, &side.id),
        Agent::Gemini => gemini::session_by_id(&gemini::config_dir(to_home), shell_cwd?, &side.id),
        Agent::Kimi => kimi::wire_of(to_home, &side.id),
    };
    found.map(|_| side.id.clone())
}

/// The line that starts `to`.  `gemini_model` is what the gemini
/// session being resumed was set to when the pane left it.
fn command(to: Agent, to_home: &Path, resume: Option<&str>, gemini_model: Option<&str>) -> Option<Vec<u8>> {
    let home = to_home.to_str()?;
    let cmd = match to {
        Agent::Claude => {
            let c = PtyCommand::new("claude").env("CLAUDE_CONFIG_DIR", home);
            // No submit receipt hook -- see
            // `receipts::claude_settings_arg` for why nothing asks for
            // one any more.
            match resume {
                Some(id) => c.arg("--resume").arg(id),
                None => c,
            }
        }
        Agent::Codex => {
            let c = PtyCommand::new("codex").env("CODEX_HOME", home);
            match resume {
                Some(id) => c.arg("resume").arg(id),
                None => c,
            }
        }
        Agent::Gemini => {
            let c = gemini::launch(to_home)?;
            let c = match gemini_model {
                Some(model) => c.arg("-m").arg(model),
                None => c,
            };
            match resume {
                Some(id) => c.arg("--resume").arg(id),
                None => c,
            }
        }
        Agent::Kimi => kimi::launch(to_home, resume)?,
    };
    cmd.clear_screen_first(true).to_bytes()
}

/// Everything the job thread decides, in one place so it can be
/// tested without a pane.
struct Outcome {
    message: Option<String>,
    next: Ledger,
}

fn prepare(sid: u64, dir: &Path, leaving: &Leaving, to: Agent, resume: Option<&str>, ledger: &Ledger)
    -> Result<Outcome, String>
{
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let obs = crate::observe::shared().latest();
    let live: Vec<u64> = obs.sessions.iter().map(|s| s.entry.id).chain(std::iter::once(sid)).collect();
    ledger::prune(dir, &live);
    let history = history_of(leaving);
    let switch = switch_note(leaving, history.as_deref());
    prepare_in(sid, dir, leaving, to, resume, ledger, history, &switch)
}

/// What the receiving agent is told about the switch.  Stopping codex
/// happens here, before its TUI is taken down: the note has to say
/// what became of it.
fn switch_note(leaving: &Leaving, history: Option<&Path>) -> transcript::Switch {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (month, day, hour, minute) = marspot::cc_usage::local_mdhm(now);
    let at = format!("{month}/{day} {hour:02}:{minute:02}");
    let thread = history.filter(|_| leaving.agent == Agent::Codex).and_then(history::codex_id_of);
    let (left, stopped) = match thread.map(|t| super::codex_stop::stop_thread(&leaving.home, &t)) {
        // claude and gemini are their process; so is a codex with no thread yet
        None => ("has exited".to_string(), true),
        Some(Ok(s)) => (left_codex(&s), true),
        Some(Err(e)) => (format!("has exited, but its background thread could not be reached ({e}) and may be carrying on"), false),
    };
    transcript::Switch { at, left, stopped }
}

fn left_codex(s: &super::codex_stop::Stopped) -> String {
    match (s.goal_paused, s.turn_interrupted) {
        (true, true) => "has exited, its goal was paused and the turn it was running was interrupted",
        (true, false) => "has exited and its goal was paused",
        (false, true) => "has exited and the turn it was running was interrupted",
        (false, false) => "has exited; it had no active goal and no turn running",
    }
    .to_string()
}

fn prepare_in(
    sid: u64,
    dir: &Path,
    leaving: &Leaving,
    to: Agent,
    resume: Option<&str>,
    ledger: &Ledger,
    history_path: Option<PathBuf>,
    switch: &transcript::Switch,
) -> Result<Outcome, String> {
    let mut next = ledger.clone();
    if resume.is_none() {
        // A fresh session is about to start; whatever the pane had
        // with `to` is not coming back.
        next.set(to, None);
    }
    let Some(path) = history_path else {
        // Nothing written yet — an agent that has not taken a turn.
        return Ok(Outcome { message: None, next });
    };
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let x = match leaving.agent {
        Agent::Claude => {
            let id = leaving.session_id.clone().unwrap_or_default();
            claude::read(&path, &id, ledger.watermark(Agent::Claude, &id)).map_err(io)?
        }
        Agent::Codex => {
            let id = history::codex_id_of(&path).unwrap_or_default();
            codex_history::read(&path, ledger.watermark(Agent::Codex, &id)).map_err(io)?
        }
        Agent::Gemini => {
            let id = leaving.session_id.clone().unwrap_or_default();
            gemini_history::read(&path, ledger.watermark(Agent::Gemini, &id)).map_err(io)?
        }
        Agent::Kimi => {
            let id = leaving.session_id.clone().unwrap_or_default();
            kimi_history::read(&path, ledger.watermark(Agent::Kimi, &id)).map_err(io)?
        }
    };
    next.set(leaving.agent, Some(Side { id: x.session_id.clone(), offset: x.end_offset }));
    if x.turns.is_empty() {
        return Ok(Outcome { message: None, next }); // nothing new since last time
    }
    let continuing = resume.is_some();
    let doc = ledger::doc_path(dir, sid);
    ledger::write_atomic(&doc, transcript::render(&x, continuing, switch).as_bytes())
        .map_err(|e| format!("{}: {e}", doc.display()))?;
    Ok(Outcome { message: Some(transcript::message(leaving.agent, &doc, continuing, switch)), next })
}

/// How long reading the history may take.  A 1.1 GB rollout is real;
/// the reader starts at the last compaction, which bounds it well
/// below that, and this is the ceiling for when it does not.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);

/// The whole switch as one scripted op.  `None` when the target's
/// command line cannot be built safely.
pub fn switch_op(sid: u64, leaving: Leaving, to: Agent, to_profile: u8, to_home: PathBuf) -> Option<PtyOp> {
    let dir = ledger::dir();
    let ledger = Ledger::load(&dir, sid);
    let shell_cwd = (to == Agent::Gemini).then(|| pidtree::proc_cwd(leaving.shell_pid)).flatten();
    let shell_cwd = shell_cwd.as_deref().and_then(Path::to_str);
    let resume = resumable(&ledger, to, &to_home, shell_cwd);
    let gemini_model =
        resume.as_deref().filter(|_| to == Agent::Gemini).and_then(|id| carried::gemini_model(&dir, sid, id));
    let line = command(to, &to_home, resume.as_deref(), gemini_model.as_deref())?;

    // What the person was typing, carried across the switch.

    let held = pty_op::Job::new();

    let job = pty_op::Job::new();
    let pending: Arc<Mutex<Option<Ledger>>> = Arc::new(Mutex::new(None));
    {
        let (j, pending, dir, leaving, resume) =
            (Arc::clone(&job), Arc::clone(&pending), dir.clone(), leaving.clone(), resume.clone());
        let spawned = std::thread::Builder::new().name("handoff".into()).spawn(move || {
            let r = prepare(sid, &dir, &leaving, to, resume.as_deref(), &ledger);
            j.finish(r.map(|o| {
                *pending.lock().unwrap() = Some(o.next);
                o.message
            }));
        });
        if let Err(e) = spawned {
            job.finish(Err(format!("spawn: {e}")));
        }
    }
    let commit: pty_op::CallFn = Arc::new(move || match pending.lock().unwrap().take() {
        Some(l) => l.save(&dir, sid).map_err(|e| format!("ledger: {e}")),
        None => Ok(()),
    });
    let predicate = match to {
        Agent::Claude => claudecode::looks_like_claudecode,
        Agent::Codex => codex::looks_like_codex,
        Agent::Gemini => gemini::looks_like_gemini,
        Agent::Kimi => kimi::looks_like_kimi,
    };
    Some(
        PtyOp::new("handoff.switch")
            .hold_screen(true)
            .badge(format!("→ {}", target_name(to, to_profile)))
            // Read before the agent is taken down. Without this the
            // handoff text pastes onto the end of whatever the person
            // had typed and the two go as one prompt -- their sentence
            // with the handover stapled on.
            .step(Step::capture_composer(Arc::clone(&held)))
            .step(Step::settle(Duration::from_millis(250)).named("hold_settle"))
            .step(Step::await_job(Arc::clone(&job)).timeout(PREPARE_TIMEOUT).named("write_handoff"))
            .step(
                Step::terminate(leaving.proc, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(Step::send(line).named("start_target"))
            // Counted from the moment the line is typed, not from when
            // the process shows up: an agent that paints its whole
            // first frame in the gap between the two would otherwise
            // leave nothing to count, and the wait would run out on a
            // screen that finished drawing long ago.  A first frame is
            // kilobytes; the echoed command line is a few hundred bytes.
            //
            // Unless the agent starts on a new conversation: its whole
            // first screen is a banner and an input box, under the
            // floor, and the wait ran out on a claude that had been
            // ready for half a minute (2026-11-10, codex to claude).
            // The line clears the screen before it starts the agent, so
            // an input box on it is the new agent's, and is enough.
            .step(
                Step::await_quiet(Duration::from_millis(1500))
                    .after_bytes(2048)
                    .or_composer()
                    .timeout(Duration::from_secs(30))
                    .named("first_frame"),
            )
            // Then make sure what drew is the agent, before typing at it.
            .step(Step::await_process(leaving.shell_pid, predicate).timeout(Duration::from_secs(5)))
            .step(Step::call(commit).named("commit_ledger"))
            .step(Step::paste_job(job).named("hand_over"))
            .step(Step::settle(Duration::from_millis(300)).named("paste_settle"))
            .step(Step::send(b"\r".to_vec()).named("submit"))
            // The handover has gone; their half-written line comes
            // back to the new agent's composer, unsent.
            .step(Step::restore_composer(held)),
    )
}

#[cfg(test)]
mod tests {
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
        let line = s(command(Agent::Kimi, Path::new("/Users/x/.kimi-code-profile-2"), Some(ID)).unwrap());
        assert!(line.contains("KIMI_CODE_HOME='/Users/x/.kimi-code-profile-2'") && line.ends_with(&format!(" kimi --session {ID}\r")), "{line}");
        assert_eq!(target_name(Agent::Kimi, 0), "kimi");
    }

    #[test]
    fn the_command_resumes_by_id_or_starts_fresh() {
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
        assert!(claude.ends_with(" --resume u-1\r"), "{claude}");
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
}

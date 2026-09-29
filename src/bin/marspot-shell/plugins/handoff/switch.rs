//! Moving a pane from one agent to the other: the menu rows, and the
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
use super::{claude, codex as codex_history, history};
use crate::plugins::pty_op::{self, PtyCommand, PtyOp, Step};
use crate::plugins::{claudecode, codex};

/// Badge-menu tags this module owns: `TAG_BASE | agent << 8 | profile`.
/// Well above the `u8` range the profile-cycle rows use, so the two
/// kinds of row cannot be mistaken for each other.
const TAG_BASE: u32 = 0x4F48_0000;

fn code(a: Agent) -> u32 {
    match a {
        Agent::Claude => 1,
        Agent::Codex => 2,
    }
}

pub fn tag_for(to: Agent, profile: u8) -> u32 {
    TAG_BASE | code(to) << 8 | profile as u32
}

/// The target a tag names, if it is one of ours.
pub fn parse_tag(tag: u32) -> Option<(Agent, u8)> {
    if tag & 0xFFFF_0000 != TAG_BASE {
        return None;
    }
    let agent = match (tag >> 8) & 0xFF {
        1 => Agent::Claude,
        2 => Agent::Codex,
        _ => return None,
    };
    Some((agent, (tag & 0xFF) as u8))
}

/// An agent's account profiles: number and directory.
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
    }
}

/// The rows a pane running `from` offers: one per profile of the other
/// agent.
pub fn menu_rows(from: Agent) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
    let to = from.other();
    profiles(to)
        .into_iter()
        .map(|(n, _)| marspot::shell_proto::PaneBadgeMenuItem {
            tag: tag_for(to, n),
            label: format!("hand off to {} P{n}", to.key()),
        })
        .collect()
}

/// The agent a pane is leaving.
#[derive(Debug, Clone)]
pub struct Leaving {
    pub agent: Agent,
    pub pid: i32,
    pub shell_pid: i32,
    /// `CLAUDE_CONFIG_DIR` / `CODEX_HOME` it runs under.
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
    }
}

/// The session the pane already has with `to`, if it can be resumed
/// from `to_home`.
fn resumable(ledger: &Ledger, to: Agent, to_home: &Path) -> Option<String> {
    let side = ledger.side(to)?;
    let found = match to {
        Agent::Claude => history::claude_transcript(to_home, &side.id),
        Agent::Codex => history::codex_rollout(to_home, &side.id),
    };
    found.map(|_| side.id.clone())
}

fn command(to: Agent, to_home: &Path, resume: Option<&str>) -> Option<Vec<u8>> {
    let home = to_home.to_str()?;
    let cmd = match to {
        Agent::Claude => {
            let c = PtyCommand::new("claude").env("CLAUDE_CONFIG_DIR", home);
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
    let live: Vec<u64> = marspot_term::session_registry::list_session_entries()
        .into_iter()
        .map(|e| e.id)
        .chain(std::iter::once(sid))
        .collect();
    ledger::prune(dir, &live);
    prepare_in(sid, dir, leaving, to, resume, ledger, history_of(leaving))
}

fn prepare_in(
    sid: u64,
    dir: &Path,
    leaving: &Leaving,
    to: Agent,
    resume: Option<&str>,
    ledger: &Ledger,
    history_path: Option<PathBuf>,
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
    };
    next.set(leaving.agent, Some(Side { id: x.session_id.clone(), offset: x.end_offset }));
    if x.turns.is_empty() {
        return Ok(Outcome { message: None, next }); // nothing new since last time
    }
    let continuing = resume.is_some();
    let doc = ledger::doc_path(dir, sid);
    ledger::write_atomic(&doc, transcript::render(&x, continuing).as_bytes())
        .map_err(|e| format!("{}: {e}", doc.display()))?;
    Ok(Outcome { message: Some(transcript::message(leaving.agent, &doc, continuing)), next })
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
    let resume = resumable(&ledger, to, &to_home);
    let line = command(to, &to_home, resume.as_deref())?;

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
    };
    Some(
        PtyOp::new("handoff.switch")
            .hold_screen(true)
            .badge(format!("→ {} P{to_profile}", to.key()))
            .step(Step::settle(Duration::from_millis(250)).named("hold_settle"))
            .step(Step::await_job(Arc::clone(&job)).timeout(PREPARE_TIMEOUT).named("write_handoff"))
            .step(
                Step::terminate(leaving.pid, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(Step::send(line).named("start_target"))
            // Counted from the moment the line is typed, not from when
            // the process shows up: an agent that paints its whole
            // first frame in the gap between the two would otherwise
            // leave nothing to count, and the wait would run out on a
            // screen that finished drawing long ago.  A first frame is
            // kilobytes; the echoed command line is a few hundred bytes.
            .step(
                Step::await_quiet(Duration::from_millis(1500))
                    .after_bytes(2048)
                    .timeout(Duration::from_secs(30))
                    .named("first_frame"),
            )
            // Then make sure what drew is the agent, before typing at it.
            .step(Step::await_process(leaving.shell_pid, predicate).timeout(Duration::from_secs(5)))
            .step(Step::call(commit).named("commit_ledger"))
            .step(Step::paste_job(job).named("hand_over"))
            .step(Step::settle(Duration::from_millis(300)).named("paste_settle"))
            .step(Step::send(b"\r".to_vec()).named("submit")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    #[test]
    fn tags_round_trip_and_stay_clear_of_profile_rows() {
        for (a, n) in [(Agent::Claude, 1u8), (Agent::Codex, 7), (Agent::Codex, 255)] {
            assert_eq!(parse_tag(tag_for(a, n)), Some((a, n)));
            assert!(u8::try_from(tag_for(a, n)).is_err(), "a profile-cycle plugin must ignore it");
        }
        assert_eq!(parse_tag(3), None);
    }

    #[test]
    fn the_command_resumes_by_id_or_starts_fresh() {
        let h = Path::new("/Users/x/.codex-profile-2");
        let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
        assert!(s(command(Agent::Codex, h, Some("t-1")).unwrap()).contains("CODEX_HOME='/Users/x/.codex-profile-2' codex resume t-1\r"));
        assert!(s(command(Agent::Codex, h, None).unwrap()).ends_with("codex\r"));
        let c = Path::new("/Users/x/.claude-profile-3");
        assert!(s(command(Agent::Claude, c, Some("u-1")).unwrap()).contains("CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' claude --resume u-1\r"));
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
            pid: 1,
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

        let o = prepare_in(9, &state, &l, Agent::Codex, None, &Ledger::default(), Some(p.clone())).unwrap();
        let msg = o.message.expect("something to hand over");
        assert!(msg.starts_with(transcript::MARKER) && msg.contains("9.md"));
        let doc = std::fs::read_to_string(ledger::doc_path(&state, 9)).unwrap();
        assert!(doc.contains("first ask"));
        assert_eq!(o.next.claude, Some(Side { id: "u-1".into(), offset: first.len() as u64 }));

        // Back in claude later: the handoff turn from codex, then a new ask.
        let more = format!("{first}{}{}", ask("b", Some("a"), "[marspot handoff] from codex"), ask("c", Some("b"), "second ask"));
        std::fs::write(&p, &more).unwrap();
        let o2 = prepare_in(9, &state, &l, Agent::Codex, Some("t-1"), &o.next, Some(p)).unwrap();
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
        let o = prepare_in(1, &d, &leaving_claude(&d, "u-1"), Agent::Codex, None, &led, Some(p)).unwrap();
        assert!(o.message.is_none());
        let o = prepare_in(1, &d, &leaving_claude(&d, "u-9"), Agent::Codex, None, &led, None).unwrap();
        assert!(o.message.is_none(), "an agent with no history hands nothing over");
    }

    /// Going fresh to an agent forgets the session the pane had there;
    /// resuming keeps it.
    #[test]
    fn a_fresh_start_forgets_the_old_session() {
        let d = tmp("switch-forget");
        let mut led = Ledger::default();
        led.set(Agent::Codex, Some(Side { id: "t-old".into(), offset: 5 }));
        let o = prepare_in(1, &d, &leaving_claude(&d, "u"), Agent::Codex, None, &led, None).unwrap();
        assert_eq!(o.next.codex, None);
        let o = prepare_in(1, &d, &leaving_claude(&d, "u"), Agent::Codex, Some("t-old"), &led, None).unwrap();
        assert_eq!(o.next.codex, led.codex);
    }

    #[test]
    fn resume_needs_the_history_to_be_reachable() {
        let d = tmp("switch-resume");
        let mut led = Ledger::default();
        led.set(Agent::Claude, Some(Side { id: "u-1".into(), offset: 0 }));
        assert_eq!(resumable(&led, Agent::Claude, &d), None);
        claude_history(&d, "u-1", "");
        assert_eq!(resumable(&led, Agent::Claude, &d).as_deref(), Some("u-1"));
    }
}

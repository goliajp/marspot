//! Moving a pane from one agent to another: the script that does it.
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
use super::note::switch_note;
use super::menu::target_name;
use super::{carried, claude, codex as codex_history, gemini as gemini_history, history, kimi as kimi_history, launch_line};
use crate::plugins::pty_op::{self, PtyOp, Step};
use crate::plugins::{claudecode, codex, gemini, helpers, kimi, pidtree};

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
    /// The pane was told its account was nearly out and to keep
    /// working, so the receiver is asked to go on with the task rather
    /// than to wait for an instruction.
    pub go_on: bool,
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
    let resume = resume.map_or(launch_line::Resume::New, launch_line::Resume::Id);
    let model = launch_line::model_for(to, to_home, resume, gemini_model);
    let l = launch_line::Launch { resume, model: model.as_deref(), flags: &[] };
    launch_line::agent_command(to, to_home, &l)?.clear_screen_first(true).to_bytes()
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
    Ok(Outcome { message: Some(transcript::message(leaving.agent, &doc, continuing, switch, leaving.go_on)), next })
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
    let pending: Arc<Mutex<Option<(Ledger, helpers::Moved)>>> = Arc::new(Mutex::new(None));
    {
        let (j, pending, dir, leaving, resume) =
            (Arc::clone(&job), Arc::clone(&pending), dir.clone(), leaving.clone(), resume.clone());
        let spawned = std::thread::Builder::new().name("handoff".into()).spawn(move || {
            let from = helpers::leaving(sid).or_else(|| leaving.session_id.clone());
            let moved = helpers::Moved { pane: sid, from, left: leaving.proc, resumed: resume.clone() };
            let r = prepare(sid, &dir, &leaving, to, resume.as_deref(), &ledger);
            j.finish(r.map(|o| {
                *pending.lock().unwrap() = Some((o.next, moved));
                o.message
            }));
        });
        if let Err(e) = spawned {
            job.finish(Err(format!("spawn: {e}")));
        }
    }
    let predicate = match to {
        Agent::Claude => claudecode::looks_like_claudecode,
        Agent::Codex => codex::looks_like_codex,
        Agent::Gemini => gemini::looks_like_gemini,
        Agent::Kimi => kimi::looks_like_kimi,
    };
    let commit: pty_op::CallFn = Arc::new(move || match pending.lock().unwrap().take() {
        Some((l, moved)) => {
            // the new agent is up, whatever becomes of the ledger
            helpers::moved(moved, leaving.shell_pid, predicate);
            l.save(&dir, sid).map_err(|e| format!("ledger: {e}"))
        }
        None => Ok(()),
    });
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
            .shell_line(leaving.shell_pid, line, "start_target")
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
                    // no or_drawn_late here: a late frame would commit the
                    // ledger past turns it never typed, losing them
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
#[path = "switch_tests.rs"]
mod tests;

//! The first moments of a new helper, on a thread of its own: answer
//! the screens a new agent puts up first, hand kimi its task, then watch
//! the screen of the agents no plugin reports turns for.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use marspot_peer::control::{AgentKind, MAX_SEND_BYTES};

use super::Shared;
use super::notify::Stop;
use super::table::Event;
use super::screen::watch_screen;
use super::work::Job;
use crate::pane_control::helpers_port::{Waited, with_port};

const CLAUDE_TRUST: &str = "Is this a project you created or one you trust";
const CLAUDE_BYPASS: &str = "Bypass Permissions mode";
const CODEX_TRUST: &str = "Trust this folder?";
/// gemini asks about the directories `--include-directories` adds even
/// with `--skip-trust`; "Yes" is the default.
const GEMINI_DIRS: &str = "Do you trust the following folders being added";

/// After the start line went in: answer the screens a new agent puts
/// up first, hand kimi its task, then watch the screen of the agents no
/// plugin reports turns for.
pub(super) fn settle(s: Arc<Shared>, sid: u64, agent: AgentKind, pos: u64) {
    let epoch = s.epoch.load(Ordering::Acquire);
    let spawned = std::thread::Builder::new().name("helpers-settle".into()).spawn(move || {
        let first = with_port(None, |p| p.wait_quiet(sid, if agent == AgentKind::Kimi { 1_500 } else { 2_000 }, Some(pos), Instant::now() + Duration::from_secs(60)));
        let up = matches!(first, Some(Ok(Waited::Met { .. })));
        match agent {
            AgentKind::Claude | AgentKind::Codex | AgentKind::Gemini if up => confirm(sid, agent),
            AgentKind::Kimi => {
                // the task file, header and all: it says where the result goes
                let task = std::fs::read_to_string(s.dir_of(sid).join("task.md")).ok();
                let delivered = up && task.as_deref().is_some_and(|t| paste_task(sid, t));
                if !delivered {
                    s.queue_wait(Job::Stopped { sid, stop: Stop::Exited, by_screen: true });
                    return;
                }
                s.record(sid, Event::TaskDelivered);
            }
            _ => {}
        }
        if matches!(agent, AgentKind::Codex | AgentKind::Gemini) {
            // from where the agent started: it may have printed all it will already
            watch_screen(&s, sid, agent, Some(pos), epoch);
        }
    });
    if let Err(e) = spawned {
        marspot::lx_warn!("helpers.settle_spawn_failed", &format!("sid={sid}: {e}"));
    }
}

/// Say yes to the folder-trust screen (claude's defaults to "No, exit",
/// so it is Down then Enter; codex's to "Trust", so Enter).
fn confirm(sid: u64, agent: AgentKind) {
    for _ in 0..2 {
        let screen = marspot::pane_read::screen(sid, 0).unwrap_or_default();
        let keys: &[&[u8]] = match agent {
            AgentKind::Claude if claude_dialog(&screen, matches!(marspot::pane_read::composer_box(sid), Ok(Some(_)))) => &[b"\x1b[B", b"\r"],
            AgentKind::Codex if screen.contains(CODEX_TRUST) => &[b"\r"],
            AgentKind::Gemini if screen.contains(GEMINI_DIRS) => &[b"\r"],
            _ => return,
        };
        let mut pos = 0;
        for k in keys {
            pos = with_port(None, |p| p.send_keys(sid, k)).and_then(Result::ok).unwrap_or(pos);
        }
        marspot::lx_info!("helpers.trust_confirmed", &format!("sid={sid} {}", agent.as_str()));
        let _ = with_port(None, |p| p.wait_quiet(sid, 2_000, Some(pos), Instant::now() + Duration::from_secs(30)));
    }
}

/// claude's trust or bypass dialog is up: its question, its own "No,
/// exit" choice on a line of its own, and no input box.  The task, once
/// it runs, is shown above the input box, so text in it that reads like
/// the dialog does not count.
pub(super) fn claude_dialog(screen: &str, composer: bool) -> bool {
    let choice = screen.lines().any(|l| {
        let l = l.trim().trim_start_matches(['❯', '>', ' ']);
        l.split_once(". ").is_some_and(|(n, rest)| n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()) && rest.trim() == "No, exit")
    });
    !composer && choice && (screen.contains(CLAUDE_TRUST) || screen.contains(CLAUDE_BYPASS))
}

/// kimi's task, pasted one send's worth at a time, Enter after the last.
fn paste_task(sid: u64, task: &str) -> bool {
    let mut rest = task.trim_end();
    while !rest.is_empty() {
        let mut cut = rest.len().min(MAX_SEND_BYTES);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (part, after) = rest.split_at(cut);
        if !with_port(None, |p| p.send_text(sid, part, after.is_empty())).is_some_and(|r| r.is_ok()) {
            return false;
        }
        rest = after;
    }
    true
}

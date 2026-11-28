//! Agent launches: one log line for every agent CLI marspot starts in a
//! pane, and one for an agent that appears in a pane without marspot.
//!
//! The command lines marspot types are its own (`PtyCommand`), so who
//! started what is read off the line itself: the pane queue shows this
//! module every line a run types at a shell (`after_typed`), and the
//! launch is logged when it is typed, not when it was planned.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use marspot::pidtree::ProcId;

use crate::plugins::pty_op::{AfterTyped, CallFn};

const AGENTS: [&str; 4] = ["claude", "codex", "gemini", "kimi"];

/// The variables an agent's account home is named by on a command line.
const HOME_KEYS: [&str; 4] = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "KIMI_CODE_HOME", "GEMINI_CLI_HOME"];

/// How long after marspot typed a launch the agent it started may take
/// to be bound to its pane: a resumed session reads its whole
/// transcript before it writes anything.
const CLAIM_WINDOW: Duration = Duration::from_secs(120);

/// Launches not yet seen in their pane; the oldest goes when full.
const PENDING_CAP: usize = 64;

static PENDING: Mutex<VecDeque<(u64, &'static str, Instant)>> = Mutex::new(VecDeque::new());

/// One agent marspot started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Launch {
    pub reason: &'static str,
    pub sid: u64,
    pub agent: &'static str,
    pub account: String,
    pub argv: String,
}

impl Launch {
    /// The launch `line` makes in pane `sid`; None when it starts no agent.
    pub(crate) fn of_line(reason: &'static str, sid: u64, line: &str) -> Option<Launch> {
        let (agent, account, argv) = describe(line)?;
        Some(Launch { reason, sid, agent, account, argv })
    }
}

/// Why a scripted run starts an agent, from the run's name.
pub(crate) fn reason_of(op_name: &'static str) -> &'static str {
    match op_name.rsplit('.').next().unwrap_or(op_name) {
        "profile_cycle" | "profile_switch" | "switch_back" => "cycle",
        "reclaim" => "reclaim",
        "cold_restore" => "cold_restore",
        "switch" => "handoff",
        "refresh" => "refresh",
        _ => op_name,
    }
}

/// The account a home path names: `P<n>` for a numbered profile.
pub(crate) fn account_of_home(home: Option<&str>) -> String {
    let Some(home) = home else { return "default".into() };
    let base = home.trim_end_matches('/').rsplit('/').next().unwrap_or(home);
    match base.rsplit_once("-profile-").and_then(|(_, n)| n.parse::<u8>().ok()) {
        Some(n) => format!("P{n}"),
        None => base.to_string(),
    }
}

/// The account a claude binding is on, by its profile number.
pub(crate) fn account_of_profile(profile: u8) -> String {
    match profile {
        0 => "default".into(),
        u8::MAX => "unknown".into(),
        n => format!("P{n}"),
    }
}

/// `(agent, account, argv)` of a command line marspot built, the
/// conversation text it carries left out.
pub(crate) fn describe(line: &str) -> Option<(&'static str, String, String)> {
    let mut s = line.trim_end_matches(['\r', '\n']);
    if s.starts_with("printf ") {
        s = &s[s.find("; ")? + 2..];
    }
    if let Some(rest) = s.strip_prefix("( cd '") {
        s = &rest[rest.find("' && ")? + 5..];
        s = s.strip_suffix(" )").unwrap_or(s);
    }
    s = s.strip_prefix("exec env ").unwrap_or(s);
    // a helper's task comes from a file: `"$(cat '…')"`
    let task = s.find(" \"$(cat ");
    let words = split_words(&s[..task.unwrap_or(s.len())]);
    let mut env: Vec<(&str, &str)> = Vec::new();
    let mut at = 0;
    while let Some(w) = words.get(at) {
        match w.split_once('=') {
            Some((k, v)) if !k.is_empty() && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') => env.push((k, v)),
            _ if w == "exec" => {}
            _ => break,
        }
        at += 1;
    }
    let program = words.get(at)?;
    let agent = *AGENTS.iter().find(|a| program.rsplit('/').next() == Some(**a))?;
    let home = env.iter().find(|(k, _)| HOME_KEYS.contains(k)).map(|(_, v)| *v);
    let mut argv = vec![agent.to_string()];
    let mut prompt = false;
    for w in &words[at + 1..] {
        if w == "--" {
            argv.push("-- <prompt>".into());
            prompt = true;
            break;
        }
        argv.push(w.clone());
    }
    if task.is_some() && !prompt {
        argv.push("<task>".into());
    }
    Some((agent, account_of_home(home), argv.join(" ")))
}

/// Words of a line in single-quote shell syntax, quotes removed.
fn split_words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in s.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Log `l` and remember it, so the agent it starts is not reported as
/// one marspot did not start.
pub(crate) fn announce(l: &Launch) {
    marspot::lx_info!(
        "launch.marspot",
        "marspot started an agent",
        reason = l.reason,
        sid = l.sid,
        agent = l.agent,
        account = &l.account,
        argv = &l.argv
    );
    let mut p = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    if p.len() >= PENDING_CAP {
        p.pop_front();
    }
    p.push_back((l.sid, l.agent, Instant::now()));
}

/// The hook the pane queue is assembled with: a line that starts an
/// agent is announced right after it is typed.
pub(crate) fn after_typed() -> AfterTyped {
    Arc::new(|sid, op_name, line| {
        let launch = Launch::of_line(reason_of(op_name), sid, &String::from_utf8_lossy(line))?;
        Some(Arc::new(move || {
            announce(&launch);
            Ok(())
        }) as CallFn)
    })
}

/// Whether marspot announced an agent `agent` in pane `sid` lately; the
/// announcement is used up.
fn claimed(sid: u64, agent: &str) -> bool {
    let mut p = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    p.retain(|(_, _, at)| at.elapsed() < CLAIM_WINDOW);
    let hit = p.iter().position(|(s, a, _)| *s == sid && *a == agent);
    hit.map(|i| p.remove(i)).is_some()
}

/// How the agent `id` in pane `sid` came to be: typed at the pane's
/// shell, or the child of something else.
fn source_of(sid: u64, id: ProcId) -> String {
    let obs = crate::observe::shared().latest();
    let shell = obs.session(sid).and_then(|s| s.shell);
    match obs.table.get(id.pid) {
        Some(r) if shell.is_some_and(|sh| sh.pid == r.ppid) => "shell".into(),
        Some(r) => format!("parent:{}", obs.table.get(r.ppid).map_or("?", |p| p.comm.as_str())),
        None => "gone".into(),
    }
}

/// The agent processes a plugin has seen in front of its panes, for
/// telling which of them marspot did not start.
#[derive(Default)]
pub(crate) struct Seen {
    primed: bool,
    /// Agents already running when the first pass ended: this process
    /// did not see them start.
    before: HashSet<ProcId>,
    procs: HashMap<u64, ProcId>,
}

impl Seen {
    /// The agent `id` is the one in front of pane `sid`, on `account`.
    pub(crate) fn saw(&mut self, sid: u64, agent: &'static str, id: ProcId, account: &str) {
        if self.procs.insert(sid, id) == Some(id) || !self.primed || self.before.contains(&id) || claimed(sid, agent) {
            return;
        }
        let source = source_of(sid, id);
        marspot::lx_info!(
            "launch.observed",
            "an agent marspot did not start appeared in a pane",
            sid = sid,
            agent = agent,
            pid = id.pid,
            account = account,
            source = &source
        );
    }

    /// A pass over the panes is done; `live` says which panes still exist.
    pub(crate) fn end_pass(&mut self, live: impl Fn(u64) -> bool) {
        self.procs.retain(|sid, _| live(*sid));
        if !self.primed {
            let obs = crate::observe::shared().latest();
            let front = obs.live().filter_map(|s| s.front()).filter(|p| AGENTS.contains(&p.name()));
            self.before = front.map(|p| p.row.id()).collect();
            self.primed = true;
        }
    }
}

#[cfg(test)]
#[path = "launches_tests.rs"]
mod tests;

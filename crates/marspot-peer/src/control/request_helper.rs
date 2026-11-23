//! The `helper.*` requests: start an agent in a shadow pane under a
//! pane, list them, stop them.

use super::{Args, bad};
use crate::control::{ErrorCode, Reject};
use crate::json::Value;

/// The longest task a helper is given.
pub const MAX_TASK_BYTES: usize = 64 * 1024;
pub const DEFAULT_BUDGET_MIN: u32 = 30;
pub const MAX_BUDGET_MIN: u32 = 360;

/// The agent command lines a helper can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
    Gemini,
    Kimi,
}

impl AgentKind {
    pub const ALL: [AgentKind; 4] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini, AgentKind::Kimi];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Gemini => "gemini",
            AgentKind::Kimi => "kimi",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    Host,
    Container,
}

impl Isolation {
    pub fn as_str(self) -> &'static str {
        match self {
            Isolation::Host => "host",
            Isolation::Container => "container",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HelperStartArgs {
    pub agent: AgentKind,
    pub task: String,
    pub cwd: Option<String>,
    pub parent: Option<String>,
    pub model: Option<String>,
    /// The account profile number; none picks one.
    pub profile: Option<u8>,
    pub isolation: Isolation,
    /// `auto`, `ask`, or flags passed to the command line as they are.
    pub mode: Option<String>,
    /// Go on with the newest conversation in `cwd`.
    pub resume_last: bool,
    pub budget_minutes: u32,
    /// A short label for people.
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HelperStop {
    /// A helper by its pane's full name (or id).
    Name(String),
    /// Every helper under the caller's pane.
    All,
}

pub(super) fn start(a: &Args) -> Result<HelperStartArgs, Reject> {
    let agent = a.str("agent")?.ok_or_else(|| bad("`agent` is required: claude, codex, gemini or kimi"))?;
    let agent = AgentKind::parse(&agent).ok_or_else(|| bad("`agent` is one of claude, codex, gemini, kimi"))?;
    let task = a.str("task")?.filter(|t| !t.trim().is_empty()).ok_or_else(|| bad("`task` is required"))?;
    if task.len() > MAX_TASK_BYTES {
        return Err(Reject::new(ErrorCode::LimitBytes, format!("{} bytes; a task is at most 64 KiB", task.len())));
    }
    let isolation = match a.str("isolation")?.as_deref() {
        None | Some("host") => Isolation::Host,
        Some("container") => Isolation::Container,
        Some(_) => return Err(bad("`isolation` is host or container")),
    };
    let resume_last = match a.str("resume")?.as_deref() {
        None => false,
        Some("last") => true,
        Some(_) => return Err(bad("`resume` is \"last\" or absent")),
    };
    let profile = match a.0.get("profile") {
        None | Some(Value::Null) => None,
        Some(Value::Num(n)) if n.fract() == 0.0 && (1.0..=255.0).contains(n) => Some(*n as u8),
        Some(Value::Str(s)) => Some(s.trim_start_matches(['P', 'p']).parse::<u8>().map_err(|_| bad("`profile` is a profile number"))?),
        Some(_) => return Err(bad("`profile` is a profile number")),
    };
    let budget_minutes = a.uint("budget_minutes")?.map_or(DEFAULT_BUDGET_MIN, |m| m.clamp(1, MAX_BUDGET_MIN as u64) as u32);
    Ok(HelperStartArgs {
        agent,
        task,
        cwd: a.str("cwd")?,
        parent: a.str("parent")?,
        model: a.str("model")?.filter(|m| !m.trim().is_empty()),
        profile,
        isolation,
        mode: a.str("mode")?.filter(|m| !m.trim().is_empty()),
        resume_last,
        budget_minutes,
        name: a.str("name")?.filter(|n| !n.trim().is_empty()),
    })
}

pub(super) fn stop(a: &Args) -> Result<HelperStop, Reject> {
    if a.bool("all", false)? {
        return Ok(HelperStop::All);
    }
    match a.str("name")?.or(a.str("target")?) {
        Some(n) if !n.trim().is_empty() => Ok(HelperStop::Name(n)),
        _ => Err(bad("give the helper's `name`, or `all:true`")),
    }
}

/// The arguments of a `helper.start` back to JSON pairs.
pub(super) fn start_json(h: &HelperStartArgs) -> Vec<(String, Value)> {
    let mut kv: Vec<(String, Value)> = vec![("agent".into(), h.agent.as_str().into()), ("task".into(), h.task.as_str().into())];
    let mut opt = |k: &str, v: &Option<String>| {
        if let Some(v) = v {
            kv.push((k.into(), v.as_str().into()));
        }
    };
    opt("cwd", &h.cwd);
    opt("parent", &h.parent);
    opt("model", &h.model);
    opt("mode", &h.mode);
    opt("name", &h.name);
    if let Some(p) = h.profile {
        kv.push(("profile".into(), i64::from(p).into()));
    }
    kv.push(("isolation".into(), h.isolation.as_str().into()));
    if h.resume_last {
        kv.push(("resume".into(), "last".into()));
    }
    kv.push(("budget_minutes".into(), i64::from(h.budget_minutes).into()));
    kv
}

pub(super) fn stop_json(s: &HelperStop) -> Vec<(String, Value)> {
    match s {
        HelperStop::All => vec![("all".into(), true.into())],
        HelperStop::Name(n) => vec![("name".into(), n.as_str().into())],
    }
}

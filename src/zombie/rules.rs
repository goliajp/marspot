//! The rules, each a pure function of what one check saw.
//!
//! Times are unix seconds, CPU is nanoseconds.  A rule never reads a
//! file or a process itself; the check hands it the readings.  A
//! reading the check could not take is `None`, and a rule with a
//! `None` where it needs a value does not hit: an unknown is never
//! taken for "silent" or "idle".

use std::path::{Component, Path, PathBuf};

use crate::pidtree::Zombie;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rule {
    /// A background task whose command a newer one of the same agent repeats word for word.
    B1,
    /// A task the agent was told had ended, still running.
    B3,
    /// A subagent nothing will wake again.
    S1,
    /// A subagent's foreground command past its own time limit.
    S2,
    /// A `tail -f` of files nobody writes.
    O1,
    /// A server outside every pane, started from a temporary place by a
    /// session that is no longer running.
    O2,
}

impl Rule {
    pub fn name(self) -> &'static str {
        match self {
            Rule::B1 => "B1",
            Rule::B3 => "B3",
            Rule::S1 => "S1",
            Rule::S2 => "S2",
            Rule::O1 => "O1",
            Rule::O2 => "O2",
        }
    }
}

/// The thresholds, `zombie.*` in settings.toml (see `limits`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub b1_silent: u64,
    pub b1_cpu_ns: u64,
    pub b1_window: u64,
    pub b3_grace: u64,
    pub s1_quiet: u64,
    pub s1_idle: u64,
    pub s2_extra: u64,
    pub s2_default_timeout: u64,
    pub o1_stale: u64,
    pub o2_age: u64,
    pub o2_cpu_ns: u64,
    pub o2_window: u64,
}

pub const MINUTE: u64 = 60;
pub const HOUR: u64 = 3600;
const MS: u64 = 1_000_000;

impl Default for Limits {
    fn default() -> Self {
        Limits {
            b1_silent: 2 * HOUR,
            b1_cpu_ns: 500 * MS,
            b1_window: 30 * MINUTE,
            b3_grace: 30 * MINUTE,
            s1_quiet: 30 * MINUTE,
            s1_idle: 10 * MINUTE,
            s2_extra: 10 * MINUTE,
            s2_default_timeout: 120,
            o1_stale: 24 * HOUR,
            o2_age: 24 * HOUR,
            o2_cpu_ns: 5_000 * MS,
            o2_window: 60 * MINUTE,
        }
    }
}

/// One background task of an agent, as its records and its processes show it.
#[derive(Clone, Debug)]
pub struct BgTask<'a> {
    pub id: &'a str,
    /// The command as the agent issued it.
    pub command: &'a str,
    /// When its root process started.
    pub started: u64,
    /// When its output file was last written; None when unknown.
    pub written: Option<u64>,
    /// CPU of its whole subtree, reaped children included, over the
    /// last `b1_window`; None until a whole window has been sampled.
    pub cpu: Option<u64>,
}

/// B1: the older copies of a background command still running in a
/// newer copy, long silent and nearly without CPU.  Each hit is the old
/// task's id and the newest copy's.
pub fn b1<'a>(tasks: &[BgTask<'a>], now: u64, l: &Limits) -> Vec<(&'a str, &'a str)> {
    let mut hits = Vec::new();
    for t in tasks {
        let newest = tasks
            .iter()
            .filter(|o| o.command == t.command)
            .max_by_key(|o| (o.started, o.id))
            .expect("the task itself is one");
        if newest.id == t.id {
            continue;
        }
        let silent = t.written.is_some_and(|w| now.saturating_sub(w) >= l.b1_silent);
        if silent && t.cpu.is_some_and(|c| c <= l.b1_cpu_ns) {
            hits.push((t.id, newest.id));
        }
    }
    hits
}

/// B3: still running `b3_grace` after the agent was told it ended.
pub fn b3(told_ended: Option<u64>, running: bool, now: u64, l: &Limits) -> bool {
    running && told_ended.is_some_and(|at| now >= at + l.b3_grace)
}

/// What a subagent's last record leaves it doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Last {
    /// Its own reply, with no tool call left open: its turn ended.
    Idle,
    /// A foreground `Bash` call it is still waiting on: when, and the
    /// call's own limit in milliseconds if it set one.
    Bash { at: u64, timeout_ms: Option<u64> },
    /// Anything else: a tool call of another kind, a result being
    /// answered, a prompt.
    Busy,
}

#[derive(Clone, Debug)]
pub struct Subagent<'a> {
    pub id: &'a str,
    /// When its record began.
    pub first: u64,
    /// When it last wrote a record.
    pub last_at: u64,
    pub last: Last,
    pub finished: bool,
    /// What it started that still runs and will wake it: background
    /// tasks with a process, subagents not finished.
    pub tasks_alive: usize,
}

/// S1: a subagent whose turn ended, of this agent process, not
/// finished, quiet for `s1_quiet`, with no task of its own left to wake
/// it, while the main session has been idle for `s1_idle`.
pub fn s1(s: &Subagent, agent_start: u64, main_ended: Option<u64>, now: u64, l: &Limits) -> bool {
    s.first >= agent_start
        && !s.finished
        && s.last == Last::Idle
        && now >= s.last_at + l.s1_quiet
        && s.tasks_alive == 0
        && main_ended.is_some_and(|at| now >= at + l.s1_idle)
}

/// S2: how long a subagent's open foreground `Bash` call is past its
/// limit plus `s2_extra`; None when it is not.
pub fn s2(s: &Subagent, now: u64, l: &Limits) -> Option<u64> {
    let Last::Bash { at, timeout_ms } = s.last else { return None };
    if s.finished {
        return None;
    }
    let limit = timeout_ms.map_or(l.s2_default_timeout, |ms| ms.div_ceil(1000));
    let due = at + limit + l.s2_extra;
    (now > due).then(|| now - at)
}

/// The files a `tail` follows, when it follows: None for a tail without
/// `-f` or `-F`, or one reading its standard input.
pub fn tail_follows(argv: &[String]) -> Option<Vec<&str>> {
    let mut follow = false;
    let mut files = Vec::new();
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        if !files.is_empty() || a == "-" || !a.starts_with('-') {
            files.push(a.as_str());
            continue;
        }
        if a == "--" {
            files.extend(args.by_ref().map(String::as_str));
            break;
        }
        for (i, c) in a.char_indices().skip(1) {
            match c {
                'f' | 'F' => follow = true,
                // the rest of the word, or the next one, is the count
                'b' | 'c' | 'n' => {
                    if i + 1 == a.len() {
                        args.next();
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    (follow && !files.is_empty() && !files.contains(&"-")).then_some(files)
}

/// O1: a following `tail` whose every file, taken from its working
/// directory, is gone or unwritten for `o1_stale`.  `written` answers
/// None for a file that is not there.
pub fn o1(argv: &[String], cwd: &Path, mut written: impl FnMut(&Path) -> Option<u64>, now: u64, l: &Limits) -> bool {
    let Some(files) = tail_follows(argv) else { return false };
    files.iter().all(|f| written(&cwd.join(f)).is_none_or(|w| now.saturating_sub(w) >= l.o1_stale))
}

/// A process outside every pane, for O2.
#[derive(Clone, Debug)]
pub struct Outside<'a> {
    pub started: u64,
    pub cwd: Option<&'a Path>,
    pub exe: Option<&'a Path>,
    /// The claude session that started it runs in no pane, along the
    /// sessions its conversation went on in.  False when it names none.
    pub session_gone: bool,
    /// Its subtree's CPU over the last `o2_window`.
    pub cpu: Option<u64>,
}

/// Where temporary things live: a claude session's own directory
/// (`/private/tmp/claude-<uid>/<project>/<session>/`), a project's
/// `.tmp` except the worktrees in it, and `$TMPDIR`.
pub fn temporary(p: &Path, uid: u32, tmpdir: Option<&Path>) -> bool {
    let p = unprivate(p);
    if tmpdir.is_some_and(|t| p.starts_with(unprivate(t))) {
        return true;
    }
    let parts: Vec<Component> = p.components().collect();
    let claude = format!("claude-{uid}");
    if parts.len() >= 5
        && parts[1].as_os_str() == "tmp"
        && parts[2].as_os_str() == claude.as_str()
        && parts[4].as_os_str().to_str().is_some_and(uuid_shaped)
    {
        return true;
    }
    parts.windows(2).any(|w| w[0].as_os_str() == ".tmp" && w[1].as_os_str() != "worktrees")
        || parts.last().is_some_and(|c| c.as_os_str() == ".tmp")
}

/// `/private/tmp/x` as `/tmp/x`: the kernel reports resolved paths,
/// `$TMPDIR` and people write the short ones.
fn unprivate(p: &Path) -> PathBuf {
    match p.strip_prefix("/private") {
        Ok(rest) if !rest.as_os_str().is_empty() => Path::new("/").join(rest),
        _ => p.to_path_buf(),
    }
}

fn uuid_shaped(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// O2: a long-running process started from a temporary place by a
/// session that is gone, doing almost nothing.
pub fn o2(o: &Outside, uid: u32, tmpdir: Option<&Path>, now: u64, l: &Limits) -> bool {
    let temp = |p: Option<&Path>| p.is_some_and(|p| temporary(p, uid, tmpdir));
    now.saturating_sub(o.started) >= l.o2_age
        && o.session_gone
        && (temp(o.cwd) || temp(o.exe))
        && o.cpu.is_some_and(|c| c <= l.o2_cpu_ns)
}

/// Z0: unreaped processes whose parent is the shell or a pane's
/// process, counted per parent, in parent order.
pub fn z0(zombies: &[Zombie], shell: i32, in_pane: impl Fn(i32) -> bool) -> Vec<(i32, usize)> {
    let mut counts: Vec<(i32, usize)> = Vec::new();
    for z in zombies.iter().filter(|z| z.ppid == shell || in_pane(z.ppid)) {
        match counts.iter_mut().find(|(p, _)| *p == z.ppid) {
            Some((_, n)) => *n += 1,
            None => counts.push((z.ppid, 1)),
        }
    }
    counts.sort_unstable();
    counts
}

#[cfg(test)]
mod tests;

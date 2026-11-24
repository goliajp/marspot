//! Which Codex thread a pane is running, and what permission class
//! that thread runs in.  Decisions only; the worker does the asking.
//!
//! A wrong binding delivers one session's mail to another, so the rule
//! is to bind only when the answer is certain and leave the pane
//! without an inbox otherwise.  Two Codex sessions in one directory are
//! common (seen on this machine while designing this), and a daemon's
//! loaded threads carry no pane or pid — only a cwd.  The one thing
//! that ties a TUI to its thread is what the TUI puts in its window
//! title when configured to (`[tui].terminal_title` with `thread-id`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use marspot_peer::envelope::Mode;
use marspot_peer::json::Value;

/// A pane with a Codex process, as the worker resolved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pane {
    pub sid: u64,
    pub home: PathBuf,
    pub cwd: String,
    /// `codex resume <id>` on the command line.
    pub argv_thread: Option<String>,
    /// The start of the thread id the TUI's window title shows.
    pub title_thread: Option<String>,
    /// The title has named something else since this process was first
    /// seen: what it names now was written by this process.
    pub title_moved: bool,
    /// The home's configuration has the title show the thread id.
    pub title_expected: bool,
    /// When the Codex process started, in milliseconds since the epoch.
    pub started_ms: u64,
    /// How long it has been running, in milliseconds.
    pub up_ms: u64,
    /// No other Codex process, in a pane or not, has been seen with
    /// this working directory since this one started.
    pub alone: bool,
}

impl Pane {
    /// The title is where this pane's thread is named.
    fn speaks_by_title(&self) -> bool {
        self.title_expected || self.title_thread.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Unbound {
    /// No loaded thread in this pane's directory that can be its own.
    NoThread,
    /// More than one candidate, and nothing that says which.
    Ambiguous { panes: usize, threads: usize },
    /// Another Codex has run in this directory since this one started,
    /// and a thread there could be either's.
    Shared,
    /// The title is where this pane's thread will be named, and it has
    /// not named one this process is known to have written.
    AwaitingTitle,
}

/// A process this old whose title names nothing has its thread told
/// without it.
const TITLE_SILENT_MS: u64 = 60_000;
/// A process opens its first thread this soon after it starts (0.169.0:
/// within two seconds).
const FIRST_THREAD_MS: u64 = 10_000;
/// A title a process has run under this long is its own.  One left by
/// an earlier process names a thread nobody shows any more, and Codex
/// unloads that after half an hour; the rest is for a machine that
/// slept through part of it.
const TITLE_SETTLED_MS: u64 = 2 * 3600 * 1000;

/// When `assign` may next answer otherwise for a pane that waits for
/// its title, with nothing having changed but the time: in so many
/// milliseconds, None when time alone settles nothing more.
pub(super) fn title_wait_ms(p: &Pane) -> Option<u64> {
    let at = if p.title_thread.is_some() { TITLE_SETTLED_MS } else { TITLE_SILENT_MS };
    at.checked_sub(p.up_ms).filter(|left| *left > 0)
}

/// When a thread was created, in milliseconds since the epoch: Codex
/// names a thread with a version 7 UUID, whose first 48 bits are that
/// time (0.163.0: equal, to the millisecond, to the `timestamp` in the
/// thread's own `session_meta`).  None for an id of any other kind,
/// and such a thread is never guessed at.  The start of an id, as
/// short as a title shows it, is enough.
pub(super) fn created_ms(thread: &str) -> Option<u64> {
    let b = thread.as_bytes();
    if b.len() < 15 || b[8] != b'-' || b[13] != b'-' || b[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&[&thread[..8], &thread[9..13]].concat(), 16).ok()
}

/// Assign threads to panes.  `threads` maps each home to its loaded
/// threads (id, cwd); `current` is the binding each pane's process
/// already has, kept while its thread is still loaded in the same
/// directory so a rescan never moves an inbox that is right.
pub(super) fn assign(
    panes: &[Pane],
    threads: &HashMap<PathBuf, Vec<(String, String)>>,
    current: &HashMap<u64, String>,
) -> BTreeMap<u64, Result<String, Unbound>> {
    let mut out: BTreeMap<u64, Result<String, Unbound>> = BTreeMap::new();
    let mut taken: Vec<(PathBuf, String)> = Vec::new();
    let candidates = |p: &Pane| -> Vec<&str> {
        threads.get(&p.home).map_or(Vec::new(), |ts| {
            ts.iter().filter(|(_, cwd)| *cwd == p.cwd).map(|(id, _)| id.as_str()).collect()
        })
    };
    // The title says which thread the TUI shows, whatever its directory
    // and whatever this pane was bound to before (`/resume` moves a TUI
    // to another thread).  It names a thread or nothing: a title naming
    // one that is not loaded leaves the pane unbound, never guessed at.
    //
    // A title is the pane's, not the process's: a Codex that was killed
    // leaves its own behind, and the next one started in that pane is
    // seen under it.  So a title counts once this process is known to
    // have written it: it names a thread created after the process
    // started, or the thread the command line resumes, or it has
    // changed since the process was first seen.
    for p in panes {
        let Some(prefix) = &p.title_thread else { continue };
        let own = created_ms(prefix).is_some_and(|c| c > p.started_ms)
            || p.argv_thread.as_ref().is_some_and(|t| t.starts_with(prefix.as_str()))
            || p.title_moved;
        if !own {
            continue;
        }
        let named: Vec<&str> = threads.get(&p.home).map_or(Vec::new(), |ts| {
            ts.iter().map(|(id, _)| id.as_str()).filter(|id| id.starts_with(prefix.as_str())).collect()
        });
        let r = match named.as_slice() {
            [] => Err(Unbound::NoThread),
            [one] => Ok(one.to_string()),
            many => Err(Unbound::Ambiguous { panes: 1, threads: many.len() }),
        };
        if let Ok(t) = &r {
            taken.push((p.home.clone(), t.clone()));
        }
        out.insert(p.sid, r);
    }
    // Then other certain answers: a binding that still holds, then a
    // thread named on the command line.
    let by_title: Vec<u64> = out.keys().copied().collect();
    let mut kept: Vec<&Pane> = Vec::new();
    for p in panes.iter().filter(|p| !by_title.contains(&p.sid)) {
        let cands: Vec<&str> =
            candidates(p).into_iter().filter(|t| !taken.iter().any(|(h, x)| *h == p.home && x == t)).collect();
        let keep = current.get(&p.sid).filter(|t| cands.contains(&t.as_str()));
        let named = p.argv_thread.as_ref().filter(|t| cands.contains(&t.as_str()));
        if let Some(t) = keep.or(named) {
            taken.push((p.home.clone(), t.clone()));
            out.insert(p.sid, Ok(t.clone()));
            // a title that has never said anything did not make it
            let silent = p.title_thread.is_none() && !p.title_moved;
            if named != Some(t) && (!p.speaks_by_title() || silent) {
                kept.push(p);
            }
        }
    }
    let unclaimed = |p: &Pane, taken: &[(PathBuf, String)]| -> Vec<String> {
        candidates(p).into_iter().filter(|t| !taken.iter().any(|(h, x)| *h == p.home && x == t)).map(str::to_string).collect()
    };
    // What could be a process's own thread without anything saying so:
    // one in its directory that nobody has, created after it started.
    // A thread stays loaded for half an hour after its TUI has gone,
    // so an older one is a past session.
    let own_unclaimed = |p: &Pane, taken: &[(PathBuf, String)]| -> Vec<String> {
        unclaimed(p, taken).into_iter().filter(|t| created_ms(t).is_some_and(|c| c > p.started_ms)).collect()
    };
    // A pairing made without a title or a command line holds for as
    // long as there is nothing else its process could be showing: a
    // second thread it could have opened (`/new`) leaves it unknown
    // which one that is.  Where the title was to say, any second
    // thread does.
    for p in kept {
        if p.speaks_by_title() {
            if !unclaimed(p, &taken).is_empty()
                && let Some(Ok(mine)) = out.insert(p.sid, Err(Unbound::AwaitingTitle))
            {
                taken.retain(|(h, t)| !(*h == p.home && *t == mine));
            }
            continue;
        }
        let others = own_unclaimed(p, &taken).len();
        if others > 0
            && let Some(Ok(mine)) = out.insert(p.sid, Err(Unbound::Ambiguous { panes: 1, threads: others + 1 }))
        {
            taken.retain(|(h, t)| !(*h == p.home && *t == mine));
        }
    }
    // Then one pane, one such thread in a directory no other Codex has
    // run in since the pane's started: that pair.  Where the title is
    // to say which thread it is, it is given its time first.
    let settled: Vec<u64> = out.keys().copied().collect();
    for p in panes.iter().filter(|p| !settled.contains(&p.sid)) {
        if p.speaks_by_title() {
            let only = p.alone && !panes.iter().any(|q| q.sid != p.sid && q.cwd == p.cwd);
            let r = match unclaimed(p, &taken).as_slice() {
                [one] if only && without_the_title(p, one) => Ok(one.clone()),
                _ => Err(Unbound::AwaitingTitle),
            };
            if let Ok(t) = &r {
                taken.push((p.home.clone(), t.clone()));
            }
            out.insert(p.sid, r);
            continue;
        }
        let rivals =
            panes.iter().filter(|q| q.sid != p.sid && !settled.contains(&q.sid) && q.home == p.home && q.cwd == p.cwd).count();
        if !p.alone || rivals > 0 {
            out.insert(p.sid, Err(Unbound::Shared));
            continue;
        }
        let r = match own_unclaimed(p, &taken).as_slice() {
            [] => Err(Unbound::NoThread),
            [one] => Ok(one.clone()),
            many => Err(Unbound::Ambiguous { panes: 1, threads: many.len() }),
        };
        if let Ok(t) = &r {
            taken.push((p.home.clone(), t.clone()));
        }
        out.insert(p.sid, r);
    }
    out
}

/// Whether `thread`, the only one in the directory of the only Codex
/// there, is that process's although its title has not been seen to
/// say so: the title it has run under for hours names it, or the title
/// names nothing and the thread is the one the process started with.
fn without_the_title(p: &Pane, thread: &str) -> bool {
    match &p.title_thread {
        Some(prefix) => p.title_expected && p.up_ms >= TITLE_SETTLED_MS && thread.starts_with(prefix.as_str()),
        None => {
            !p.title_moved
                && p.up_ms >= TITLE_SILENT_MS
                && created_ms(thread).is_some_and(|c| c > p.started_ms && c - p.started_ms <= FIRST_THREAD_MS)
        }
    }
}

/// A loaded thread's cwd, if it is one a person is talking to in a
/// TUI.  Codex also keeps threads of its own loaded in the same
/// directory — a title generator (`ephemeral`, `threadSource:
/// "thread_title"`, seen alongside a fresh session), sub-agents (a
/// parent thread) — and counting those would leave every pane
/// ambiguous.  An unknown source is not a user's: the cost of a wrong
/// "no" is a pane without an inbox, of a wrong "yes" a misdelivery.
pub(super) fn user_thread_cwd(thread: &Value) -> Option<String> {
    let ephemeral = thread.get("ephemeral").and_then(Value::bool).unwrap_or(false);
    let child = thread.get("parentThreadId").is_some_and(|p| *p != Value::Null);
    let source_ok = match thread.get("threadSource") {
        None | Some(Value::Null) => true,
        Some(v) => v.str() == Some("user"),
    };
    if ephemeral || child || !source_ok {
        return None;
    }
    thread.str_at("cwd").map(str::to_string)
}

/// The class a thread runs in, from `thread/resume`'s
/// `approvalPolicy` + `sandbox`, or `config/read`'s `approval_policy` +
/// `sandbox_mode`.  Bypass only when approvals are off and the sandbox
/// is full access; anything else, or anything unrecognised, prompts.
pub(super) fn class_of(approval: Option<&Value>, sandbox: Option<&Value>) -> Mode {
    let never = approval.and_then(Value::str) == Some("never");
    let full = match sandbox {
        Some(Value::Str(s)) => s == "danger-full-access",
        Some(v) => v.str_at("type") == Some("dangerFullAccess"),
        None => false,
    };
    if never && full { Mode::Bypass } else { Mode::Prompting }
}

/// What a TUI's own command line overrides, for a thread that has not
/// run a turn yet (its settings cannot be read back until it has).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ArgvOverride {
    pub approval: Option<String>,
    pub sandbox: Option<String>,
    /// `-p` / `--profile` selects a config layer this side cannot read.
    pub opaque: bool,
}

pub(super) fn argv_override(argv: &[String]) -> ArgvOverride {
    let mut o = ArgvOverride::default();
    let mut it = argv.iter().skip(1).map(String::as_str);
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (a, None),
        };
        let mut val = || inline.map(str::to_string).or_else(|| it.next().map(str::to_string));
        match flag {
            "--yolo" | "--dangerously-bypass-approvals-and-sandbox" => {
                o.approval = Some("never".into());
                o.sandbox = Some("danger-full-access".into());
            }
            "--full-auto" => {
                o.approval = Some("on-request".into());
                o.sandbox = Some("workspace-write".into());
            }
            "-a" | "--ask-for-approval" => o.approval = val(),
            "-s" | "--sandbox" => o.sandbox = val(),
            "-p" | "--profile" => o.opaque = true,
            "-c" | "--config" => {
                if let Some(kv) = val()
                    && let Some((k, v)) = kv.split_once('=')
                {
                    let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
                    match k.trim() {
                        "approval_policy" => o.approval = Some(v),
                        "sandbox_mode" => o.sandbox = Some(v),
                        "profile" => o.opaque = true,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::peerbus::title::thread_in_title;
    use marspot_peer::json::parse;

    // threads created after a process that started at 0
    const N1: &str = "00000001-0000-7000-8000-000000000001";
    const N2: &str = "00000001-0000-7000-8000-000000000002";
    const N3: &str = "00000001-0000-7000-8000-000000000003";

    fn pane(sid: u64, cwd: &str, argv: Option<&str>) -> Pane {
        Pane {
            sid,
            home: "/h".into(),
            cwd: cwd.into(),
            argv_thread: argv.map(str::to_string),
            title_thread: None,
            title_moved: false,
            title_expected: false,
            started_ms: 0,
            up_ms: 0,
            alone: true,
        }
    }
    /// A thread id created at `ms`.
    fn born(ms: u64, n: u64) -> String {
        format!("{:08x}-{:04x}-7000-8000-{n:012x}", ms >> 16, ms & 0xffff)
    }

    #[test]
    fn a_thread_id_carries_when_it_was_created() {
        // a thread of Codex 0.163.0 and the timestamp in its session_meta
        assert_eq!(created_ms("01a1d6f4-d181-72f3-be32-8f81526d5bdf"), Some(1_794_607_731_073));
        assert_eq!(created_ms("01a1d6f4-d181-72f3-be32-8f815"), Some(1_794_607_731_073), "as much as a title shows");
        assert_eq!(created_ms(&born(1_794_607_731_073, 9)), Some(1_794_607_731_073));
        assert_eq!(created_ms("01a1d6f4-d181-42f3-be32-8f81526d5bdf"), None, "a random UUID says nothing");
        assert_eq!(created_ms("t1"), None);
    }

    /// A Codex quit ten seconds ago; its thread is still loaded.  The
    /// one started now in the same directory has none of its own yet.
    #[test]
    fn one_pane_and_one_thread_older_than_its_process_stays_unbound() {
        let stale = born(51_000, 1);
        let p = Pane { started_ms: 61_000, ..pane(2, "/p", None) };
        let ts = loaded(&[(stale.as_str(), "/p")]);
        assert_eq!(assign(std::slice::from_ref(&p), &ts, &HashMap::new())[&2], Err(Unbound::NoThread));
        // its own arrives: that one, and only that one
        let own = born(65_000, 2);
        let ts = loaded(&[(stale.as_str(), "/p"), (own.as_str(), "/p")]);
        assert_eq!(assign(&[p], &ts, &HashMap::new())[&2], Ok(own));
    }

    #[test]
    fn a_thread_is_a_processs_own_only_when_created_after_it_started() {
        let t = born(5_000, 1);
        let ts = loaded(&[(t.as_str(), "/p")]);
        let at = |started_ms| assign(&[Pane { started_ms, ..pane(1, "/p", None) }], &ts, &HashMap::new())[&1].clone();
        assert_eq!(at(4_999), Ok(t.clone()));
        assert_eq!(at(5_000), Err(Unbound::NoThread), "the same millisecond is not after");
        assert_eq!(at(5_001), Err(Unbound::NoThread));
        // an id that carries no time is never guessed at
        let ts = loaded(&[("0f0e0d0c-0b0a-4000-8000-000000000001", "/p")]);
        assert_eq!(assign(&[pane(1, "/p", None)], &ts, &HashMap::new())[&1], Err(Unbound::NoThread));
    }

    /// Where the title is to show the thread, a pane waits for it,
    /// however certain the pairing looks, until its time is up.
    #[test]
    fn nothing_is_guessed_where_the_title_will_say() {
        let ts = loaded(&[(N1, "/p")]);
        let p = Pane { title_expected: true, ..pane(1, "/p", None) };
        assert_eq!(assign(std::slice::from_ref(&p), &ts, &HashMap::new())[&1], Err(Unbound::AwaitingTitle));
        let shown = Pane { title_thread: thread_in_title(&format!("p | {N1}")), ..p.clone() };
        assert_eq!(assign(&[shown], &ts, &HashMap::new())[&1], Ok(N1.into()));
        // a binding its process already has is not a guess
        assert_eq!(assign(std::slice::from_ref(&p), &ts, &HashMap::from([(1, N1.to_string())]))[&1], Ok(N1.into()));
        // nor is the thread its command line resumes
        assert_eq!(assign(&[Pane { argv_thread: Some(N1.into()), ..p }], &ts, &HashMap::new())[&1], Ok(N1.into()));
    }

    /// A Codex whose title never says anything, alone in its directory.
    #[test]
    fn a_title_that_stays_silent_gives_way_to_the_one_thread_there_is() {
        let first = born(1_001_500, 1);
        let ts = loaded(&[(first.as_str(), "/p")]);
        let p = Pane { title_expected: true, started_ms: 1_000_000, up_ms: TITLE_SILENT_MS, ..pane(1, "/p", None) };
        let one = |p: &Pane, ts: &_| assign(std::slice::from_ref(p), ts, &HashMap::new())[&1].clone();
        assert_eq!(one(&p, &ts), Ok(first.clone()));
        // its time is not up
        let early = Pane { up_ms: TITLE_SILENT_MS - 1, ..p.clone() };
        assert_eq!(one(&early, &ts), Err(Unbound::AwaitingTitle));
        assert_eq!(title_wait_ms(&early), Some(1));
        assert_eq!(title_wait_ms(&p), None);
        // another codex has run there
        assert_eq!(one(&Pane { alone: false, ..p.clone() }, &ts), Err(Unbound::AwaitingTitle));
        // a second thread, whatever its age
        let second = loaded(&[(first.as_str(), "/p"), (T1, "/p")]);
        assert_eq!(one(&p, &second), Err(Unbound::AwaitingTitle));
        // not the thread it started with
        let late = born(1_000_000 + FIRST_THREAD_MS + 1, 1);
        assert_eq!(one(&p, &loaded(&[(late.as_str(), "/p")])), Err(Unbound::AwaitingTitle));
        let before = born(999_000, 1);
        assert_eq!(one(&p, &loaded(&[(before.as_str(), "/p")])), Err(Unbound::AwaitingTitle));
        // it has named another since, and says nothing now
        assert_eq!(one(&Pane { title_moved: true, ..p.clone() }, &ts), Err(Unbound::AwaitingTitle));
        // two panes in the directory
        let r = assign(&[p.clone(), Pane { sid: 2, ..p.clone() }], &ts, &HashMap::new());
        assert_eq!((&r[&1], &r[&2]), (&Err(Unbound::AwaitingTitle), &Err(Unbound::AwaitingTitle)));
    }

    #[test]
    fn a_pairing_made_under_a_silent_title_yields_to_a_second_thread_and_to_the_title() {
        let first = born(1_001_500, 1);
        let cur = HashMap::from([(1, first.clone())]);
        let p = Pane { title_expected: true, started_ms: 1_000_000, up_ms: TITLE_SILENT_MS, ..pane(1, "/p", None) };
        let one = loaded(&[(first.as_str(), "/p")]);
        assert_eq!(assign(std::slice::from_ref(&p), &one, &cur)[&1], Ok(first.clone()));
        let old = born(51_000, 9);
        let two = loaded(&[(first.as_str(), "/p"), (old.as_str(), "/p")]);
        assert_eq!(assign(std::slice::from_ref(&p), &two, &cur)[&1], Err(Unbound::AwaitingTitle));
        let says = Pane { title_thread: thread_in_title(&format!("p | {old}")), title_moved: true, ..p };
        assert_eq!(assign(&[says], &two, &cur)[&1], Ok(old));
    }

    /// A `codex` started bare, moved to an older thread with `/resume`,
    /// idle since; what was known of its title went with a restart.
    #[test]
    fn a_title_a_process_has_run_under_for_hours_is_its_own() {
        let old = "01a0bca3-87a2-7aa2-8d42-d84a71b8539a";
        let ts = loaded(&[(old, "/p")]);
        let p = Pane {
            title_expected: true,
            title_thread: thread_in_title("plan | p | 01a0bca3-87a2-7aa2-8d42-d84a7..."),
            started_ms: 1_795_008_854_000,
            up_ms: TITLE_SETTLED_MS,
            ..pane(1, "/p", None)
        };
        let one = |p: &Pane, ts: &_| assign(std::slice::from_ref(p), ts, &HashMap::new())[&1].clone();
        assert_eq!(one(&p, &ts), Ok(old.into()));
        let early = Pane { up_ms: TITLE_SETTLED_MS - 1, ..p.clone() };
        assert_eq!(one(&early, &ts), Err(Unbound::AwaitingTitle));
        assert_eq!(title_wait_ms(&early), Some(1));
        assert_eq!(one(&Pane { alone: false, ..p.clone() }, &ts), Err(Unbound::AwaitingTitle));
        assert_eq!(one(&Pane { title_expected: false, ..p.clone() }, &ts), Err(Unbound::AwaitingTitle));
        assert_eq!(one(&p, &loaded(&[(old, "/q")])), Err(Unbound::AwaitingTitle), "a thread of another directory");
        // the thread it started with is there too: which one it shows is not said
        let first = born(1_795_008_855_013, 1);
        assert_eq!(one(&p, &loaded(&[(old, "/p"), (first.as_str(), "/p")])), Err(Unbound::AwaitingTitle));
        // and alone, it is not what the title names
        assert_eq!(one(&p, &loaded(&[(first.as_str(), "/p")])), Err(Unbound::AwaitingTitle));
        // another pane has it by its own title
        let other = Pane { sid: 2, cwd: "/q".into(), title_moved: true, ..p.clone() };
        let r = assign(&[p, other], &ts, &HashMap::new());
        assert_eq!((&r[&1], &r[&2]), (&Err(Unbound::AwaitingTitle), &Ok(old.to_string())));
    }

    /// The title a killed Codex left in the pane names its thread, and
    /// that thread is still loaded.
    #[test]
    fn a_title_left_by_an_earlier_process_is_not_this_ones() {
        let left = born(51_000, 1);
        let ts = loaded(&[(left.as_str(), "/p")]);
        let p = Pane { started_ms: 61_000, title_thread: thread_in_title(&format!("p | {left}")), ..pane(1, "/p", None) };
        assert_eq!(assign(std::slice::from_ref(&p), &ts, &HashMap::new())[&1], Err(Unbound::AwaitingTitle));
        // the process writes its own
        let own = born(65_000, 2);
        let ts = loaded(&[(left.as_str(), "/p"), (own.as_str(), "/p")]);
        let wrote = Pane { title_thread: thread_in_title(&format!("p | {own}")), title_moved: true, ..p.clone() };
        assert_eq!(assign(&[wrote], &ts, &HashMap::new())[&1], Ok(own));
        // and `/resume` back to the older one is then its own doing
        let back = Pane { title_moved: true, ..p.clone() };
        assert_eq!(assign(&[back], &ts, &HashMap::new())[&1], Ok(left.clone()));
        // `codex resume <id>` shows an older thread from the start
        let resumed = Pane { argv_thread: Some(left.clone()), ..p };
        assert_eq!(assign(&[resumed], &ts, &HashMap::new())[&1], Ok(left));
    }

    fn titled(sid: u64, cwd: &str, title: &str) -> Pane {
        Pane { title_thread: thread_in_title(title), ..pane(sid, cwd, None) }
    }
    fn loaded(ts: &[(&str, &str)]) -> HashMap<PathBuf, Vec<(String, String)>> {
        HashMap::from([(PathBuf::from("/h"), ts.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect())])
    }

    #[test]
    fn one_pane_one_thread_binds() {
        let r = assign(&[pane(1, "/p", None)], &loaded(&[(N1, "/p"), (N2, "/q")]), &HashMap::new());
        assert_eq!(r[&1], Ok(N1.into()));
    }

    #[test]
    fn two_panes_in_one_directory_stay_unbound_without_ids() {
        let ts = loaded(&[(N1, "/p"), (N2, "/p")]);
        let r = assign(&[pane(1, "/p", None), pane(2, "/p", None)], &ts, &HashMap::new());
        assert_eq!(r[&1], Err(Unbound::Shared));
        assert_eq!(r[&2], Err(Unbound::Shared));
    }

    /// One thread in the directory, created after this pane's Codex
    /// started, and nobody has it: it is still not this pane's when
    /// another Codex has run there in the meantime, in a pane or not.
    #[test]
    fn nothing_is_paired_in_a_directory_another_codex_has_run_in() {
        let ts = loaded(&[(N1, "/p")]);
        let p = Pane { alone: false, ..pane(1, "/p", None) };
        assert_eq!(assign(std::slice::from_ref(&p), &ts, &HashMap::new())[&1], Err(Unbound::Shared));
        assert_eq!(assign(&[Pane { alone: true, ..p.clone() }], &ts, &HashMap::new())[&1], Ok(N1.into()));
        // a title settles it all the same
        let shown = Pane { title_thread: thread_in_title(&format!("p | {N1}")), ..p };
        assert_eq!(assign(&[shown], &ts, &HashMap::new())[&1], Ok(N1.into()));
    }

    /// Paired while it was alone; then a second thread its process
    /// could have opened appears.  Which one the TUI shows is no
    /// longer known, and the pairing is not kept on a guess.
    #[test]
    fn a_pairing_is_given_up_when_a_second_thread_could_be_the_processs() {
        let cur = HashMap::from([(1, N1.to_string())]);
        let p = pane(1, "/p", None);
        assert_eq!(assign(std::slice::from_ref(&p), &loaded(&[(N1, "/p")]), &cur)[&1], Ok(N1.into()));
        let two = loaded(&[(N1, "/p"), (N2, "/p")]);
        assert_eq!(assign(std::slice::from_ref(&p), &two, &cur)[&1], Err(Unbound::Ambiguous { panes: 1, threads: 2 }));
        // one older than the process is not its own, and changes nothing
        let late = Pane { started_ms: 70_000, ..p.clone() };
        let older = born(51_000, 9);
        let cur = HashMap::from([(1, born(80_000, 1))]);
        let ts = loaded(&[(cur[&1].as_str(), "/p"), (older.as_str(), "/p")]);
        assert_eq!(assign(&[late], &ts, &cur)[&1], Ok(cur[&1].clone()));
        // a thread the command line names is kept whatever else is there
        let cur = HashMap::from([(1, N1.to_string())]);
        assert_eq!(assign(&[Pane { argv_thread: Some(N1.into()), ..p }], &two, &cur)[&1], Ok(N1.into()));
    }

    /// A stale loaded thread in the same directory must not be guessed
    /// at either.
    #[test]
    fn one_pane_two_threads_stays_unbound() {
        let r = assign(&[pane(1, "/p", None)], &loaded(&[(N1, "/p"), (N2, "/p")]), &HashMap::new());
        assert_eq!(r[&1], Err(Unbound::Ambiguous { panes: 1, threads: 2 }));
    }

    #[test]
    fn a_named_thread_settles_its_pane_and_not_the_other_in_its_directory() {
        let ts = loaded(&[(N1, "/p"), (N2, "/p")]);
        let other = Pane { alone: false, ..pane(2, "/p", None) };
        let r = assign(&[pane(1, "/p", Some(N2)), other], &ts, &HashMap::new());
        assert_eq!(r[&1], Ok(N2.into()));
        assert_eq!(r[&2], Err(Unbound::Shared), "the thread left over is not the other pane's for being left over");
    }

    #[test]
    fn an_existing_binding_is_kept_and_dropped_when_its_thread_goes() {
        let ts = loaded(&[(N1, "/p"), (N2, "/q")]);
        let cur = HashMap::from([(1, N1.to_string())]);
        assert_eq!(assign(&[pane(1, "/p", None)], &ts, &cur)[&1], Ok(N1.into()));
        let gone = loaded(&[(N3, "/p")]);
        assert_eq!(assign(&[pane(1, "/p", None)], &gone, &cur)[&1], Ok(N3.into()));
        assert_eq!(assign(&[pane(1, "/p", None)], &loaded(&[]), &cur)[&1], Err(Unbound::NoThread));
    }

    const T1: &str = "01a161ee-50e9-7b50-8ad6-4bcae0a1b2c3";
    const T2: &str = "01a161ee-50e9-7b50-8ad6-9f00d0a1b2c3";

    /// Two TUIs in one directory, one of them freshly reopened while the
    /// thread it left waits to unload: the titles settle all of it.
    #[test]
    fn titles_bind_panes_that_share_a_directory() {
        let stale = "01a10000-0000-7000-8000-000000000000";
        let ts = loaded(&[(T1, "/p"), (T2, "/p"), (stale, "/p")]);
        let panes = [titled(1, "/p", "p | 01a161ee-50e9-7b50-8ad6-4bcae..."), titled(2, "/p", &format!("p | {T2}"))];
        let r = assign(&panes, &ts, &HashMap::new());
        assert_eq!(r[&1], Ok(T1.into()));
        assert_eq!(r[&2], Ok(T2.into()));
    }

    #[test]
    fn a_title_overrides_an_old_binding_and_names_nothing_it_cannot_find() {
        let ts = loaded(&[(T1, "/p"), (T2, "/q")]);
        let cur = HashMap::from([(1, T1.to_string())]);
        let moved = titled(1, "/p", &format!("p | {T2}"));
        assert_eq!(assign(&[moved], &ts, &cur)[&1], Ok(T2.into()), "another directory is fine");
        let gone = titled(1, "/p", "p | 01a19999-0000-7000-8000-0000...");
        assert_eq!(assign(&[gone], &ts, &cur)[&1], Err(Unbound::NoThread));
        let short = Pane { title_thread: Some("01a161ee-50e9-7b50-8ad6-".into()), ..pane(1, "/p", None) };
        assert_eq!(assign(&[short], &ts, &cur)[&1], Err(Unbound::Ambiguous { panes: 1, threads: 2 }));
    }

    /// A pane without a title id does not get the thread a title claimed.
    #[test]
    fn a_thread_claimed_by_a_title_is_not_given_to_another_pane() {
        let ts = loaded(&[(T1, "/p")]);
        let cur = HashMap::from([(2, T1.to_string())]);
        let r = assign(&[titled(1, "/p", &format!("p | {T1}")), pane(2, "/p", Some(T1))], &ts, &cur);
        assert_eq!(r[&1], Ok(T1.into()));
        assert_eq!(r[&2], Err(Unbound::NoThread));
    }

    /// Field values from Codex 0.163.0's own threads in one directory.
    #[test]
    fn only_a_users_thread_counts() {
        let v = |s: &str| parse(s).unwrap();
        let user = v(r#"{"cwd":"/p","ephemeral":false,"parentThreadId":null,"threadSource":"user"}"#);
        let title = v(r#"{"cwd":"/p","ephemeral":true,"parentThreadId":null,"threadSource":"thread_title"}"#);
        let child = v(r#"{"cwd":"/p","ephemeral":false,"parentThreadId":"01a1","threadSource":"user"}"#);
        let other = v(r#"{"cwd":"/p","ephemeral":false,"threadSource":"review"}"#);
        let bare = v(r#"{"cwd":"/p"}"#);
        assert_eq!(user_thread_cwd(&user).as_deref(), Some("/p"));
        assert_eq!(user_thread_cwd(&title), None);
        assert_eq!(user_thread_cwd(&child), None);
        assert_eq!(user_thread_cwd(&other), None);
        assert_eq!(user_thread_cwd(&bare).as_deref(), Some("/p"));
    }

    #[test]
    fn class_needs_both_never_and_full_access() {
        let v = |s: &str| parse(s).unwrap();
        assert_eq!(class_of(Some(&v("\"never\"")), Some(&v(r#"{"type":"dangerFullAccess"}"#))), Mode::Bypass);
        assert_eq!(class_of(Some(&v("\"never\"")), Some(&v("\"danger-full-access\""))), Mode::Bypass);
        assert_eq!(class_of(Some(&v("\"on-request\"")), Some(&v("\"danger-full-access\""))), Mode::Prompting);
        assert_eq!(class_of(Some(&v("\"never\"")), Some(&v(r#"{"type":"workspaceWrite"}"#))), Mode::Prompting);
        assert_eq!(class_of(None, None), Mode::Prompting);
        assert_eq!(class_of(Some(&v(r#"{"granular":{}}"#)), Some(&v("\"danger-full-access\""))), Mode::Prompting);
    }

    #[test]
    fn argv_overrides_are_read() {
        let a = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(argv_override(&a("codex --yolo")).approval.as_deref(), Some("never"));
        let o = argv_override(&a("codex -a on-request -c sandbox_mode=\"read-only\""));
        assert_eq!((o.approval.as_deref(), o.sandbox.as_deref()), (Some("on-request"), Some("read-only")));
        assert_eq!(argv_override(&a("codex --sandbox=workspace-write")).sandbox.as_deref(), Some("workspace-write"));
        assert!(argv_override(&a("codex -p work")).opaque);
        assert_eq!(argv_override(&a("codex resume 01a1")), ArgvOverride::default());
    }
}

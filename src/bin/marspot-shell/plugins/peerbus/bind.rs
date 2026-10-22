//! Which Codex thread a pane is running, and what permission class
//! that thread runs in.  Decisions only; the worker does the asking.
//!
//! A wrong binding delivers one session's mail to another, so the rule
//! is to bind only when the answer is certain and leave the pane
//! without an inbox otherwise.  Two Codex sessions in one directory are
//! common (seen on this machine while designing this), and a daemon's
//! loaded threads carry no pane or pid — only a cwd.

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Unbound {
    /// No loaded thread in this pane's directory.
    NoThread,
    /// More than one candidate, and nothing that says which.
    Ambiguous { panes: usize, threads: usize },
}

/// Assign threads to panes.  `threads` maps each home to its loaded
/// threads (id, cwd); `current` is the binding each pane already has,
/// kept while its thread is still loaded in the same directory so a
/// rescan never moves an inbox that is right.
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
    // Certain answers first: a binding that still holds, then a thread
    // named on the command line.
    for p in panes {
        let cands = candidates(p);
        let keep = current.get(&p.sid).filter(|t| cands.contains(&t.as_str()));
        let named = p.argv_thread.as_ref().filter(|t| cands.contains(&t.as_str()));
        if let Some(t) = keep.or(named) {
            taken.push((p.home.clone(), t.clone()));
            out.insert(p.sid, Ok(t.clone()));
        }
    }
    // Then one pane, one thread left in a directory: that pair.
    let settled: Vec<u64> = out.keys().copied().collect();
    for p in panes.iter().filter(|p| !settled.contains(&p.sid)) {
        let free: Vec<&str> =
            candidates(p).into_iter().filter(|t| !taken.iter().any(|(h, x)| *h == p.home && x == t)).collect();
        let rivals = panes
            .iter()
            .filter(|q| !settled.contains(&q.sid) && q.home == p.home && q.cwd == p.cwd)
            .count();
        let r = match (free.as_slice(), rivals) {
            ([], _) => Err(Unbound::NoThread),
            ([one], 1) => Ok(one.to_string()),
            (many, n) => Err(Unbound::Ambiguous { panes: n, threads: many.len() }),
        };
        if let Ok(t) = &r {
            taken.push((p.home.clone(), t.clone()));
        }
        out.insert(p.sid, r);
    }
    out
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

/// The registry name for a bound thread: `codex-<project>`, made
/// unique against names already taken.
pub(super) fn record_name(cwd: &str, thread: &str, taken: &[String]) -> String {
    let base: String = cwd
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("session")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let name = format!("codex-{base}");
    if !taken.contains(&name) {
        return name;
    }
    let short: String = thread.chars().filter(char::is_ascii_hexdigit).rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{name}-{short}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json::parse;

    fn pane(sid: u64, cwd: &str, argv: Option<&str>) -> Pane {
        Pane { sid, home: "/h".into(), cwd: cwd.into(), argv_thread: argv.map(str::to_string) }
    }
    fn loaded(ts: &[(&str, &str)]) -> HashMap<PathBuf, Vec<(String, String)>> {
        HashMap::from([(PathBuf::from("/h"), ts.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect())])
    }

    #[test]
    fn one_pane_one_thread_binds() {
        let r = assign(&[pane(1, "/p", None)], &loaded(&[("t1", "/p"), ("t2", "/q")]), &HashMap::new());
        assert_eq!(r[&1], Ok("t1".into()));
    }

    #[test]
    fn two_panes_in_one_directory_stay_unbound_without_ids() {
        let ts = loaded(&[("t1", "/p"), ("t2", "/p")]);
        let r = assign(&[pane(1, "/p", None), pane(2, "/p", None)], &ts, &HashMap::new());
        assert_eq!(r[&1], Err(Unbound::Ambiguous { panes: 2, threads: 2 }));
        assert_eq!(r[&2], Err(Unbound::Ambiguous { panes: 2, threads: 2 }));
    }

    /// A stale loaded thread in the same directory must not be guessed
    /// at either.
    #[test]
    fn one_pane_two_threads_stays_unbound() {
        let r = assign(&[pane(1, "/p", None)], &loaded(&[("t1", "/p"), ("t2", "/p")]), &HashMap::new());
        assert_eq!(r[&1], Err(Unbound::Ambiguous { panes: 1, threads: 2 }));
    }

    #[test]
    fn a_named_thread_settles_its_pane_and_frees_the_other() {
        let ts = loaded(&[("t1", "/p"), ("t2", "/p")]);
        let r = assign(&[pane(1, "/p", Some("t2")), pane(2, "/p", None)], &ts, &HashMap::new());
        assert_eq!(r[&1], Ok("t2".into()));
        assert_eq!(r[&2], Ok("t1".into()));
    }

    #[test]
    fn an_existing_binding_is_kept_and_dropped_when_its_thread_goes() {
        let ts = loaded(&[("t1", "/p"), ("t2", "/p")]);
        let cur = HashMap::from([(1, "t1".to_string())]);
        assert_eq!(assign(&[pane(1, "/p", None)], &ts, &cur)[&1], Ok("t1".into()));
        let gone = loaded(&[("t3", "/p")]);
        assert_eq!(assign(&[pane(1, "/p", None)], &gone, &cur)[&1], Ok("t3".into()));
        assert_eq!(assign(&[pane(1, "/p", None)], &loaded(&[]), &cur)[&1], Err(Unbound::NoThread));
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

    #[test]
    fn names_are_made_unique() {
        assert_eq!(record_name("/Users/x/workspace/goliajp/spg", "t", &[]), "codex-spg");
        let taken = vec!["codex-spg".to_string()];
        assert_eq!(record_name("/w/spg/", "01a16103-3f4c-7883-b4ee-2a471b88dda9", &taken), "codex-spg-dda9");
        assert_eq!(record_name("/w/My Proj", "t", &[]), "codex-my-proj");
    }
}

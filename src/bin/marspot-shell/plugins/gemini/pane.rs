//! The gemini process behind a pane: what its argv says, which home it
//! runs under, and how to move it to another one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::looks_like_gemini;
use crate::plugins::{pidtree, pty_op};

/// What a gemini command line says about its session and model.
#[derive(Debug, Default, PartialEq)]
pub(super) struct ArgvFacts {
    /// `--session-id <id>` (a new session under that id) or
    /// `--resume <uuid>`.  `--resume latest` and `--resume <index>` name
    /// no session by themselves and leave this empty.
    pub session: Option<String>,
    /// `-m` / `--model`, as typed: an alias (`flash`) stays an alias.
    pub model: Option<String>,
}

/// An id that can go on a command line and into a file-name match:
/// gemini itself accepts `[a-zA-Z0-9-_]` for `--session-id`.
pub(super) fn is_session_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.bytes().filter(|b| *b == b'-').count() == 4 && is_session_id(s)
}

fn is_model(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_graphic())
}

/// Read the flags off `argv`.  Both spellings yargs accepts: the value
/// as the next argument, or after `=`.
pub(super) fn argv_facts(argv: &[String]) -> ArgvFacts {
    let mut facts = ArgvFacts::default();
    let mut it = argv.iter().skip(1).map(String::as_str);
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg, None),
        };
        if !matches!(flag, "--session-id" | "--resume" | "-r" | "--model" | "-m") {
            continue;
        }
        let Some(value) = inline.or_else(|| it.next()) else { break };
        match flag {
            "--session-id" if is_session_id(value) => facts.session = Some(value.to_string()),
            "--resume" | "-r" if is_uuid(value) => facts.session = Some(value.to_string()),
            "--model" | "-m" if is_model(value) => facts.model = Some(value.to_string()),
            _ => {}
        }
    }
    facts
}

/// The directory gemini treats as home: its `GEMINI_CLI_HOME`, or the
/// user's own.  An empty value is unset to gemini, so it is here too.
pub(super) fn root_from(gemini_cli_home: Option<String>, user_home: &Path) -> PathBuf {
    match gemini_cli_home {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => user_home.to_path_buf(),
    }
}

/// Where gemini keeps its state under a home.  `GEMINI_CLI_HOME`
/// replaces the HOME, not the config directory — one level more than
/// `CLAUDE_CONFIG_DIR` and `CODEX_HOME`, which name the directory itself.
pub(crate) fn config_dir(root: &Path) -> PathBuf {
    root.join(".gemini")
}

/// Account profiles, the `GEMINI_CLI_HOME` kind: `~/.gemini-profile-N`
/// is a whole home with its own login, settings and sessions, the same
/// convention the claude and codex profiles follow.
const GEMINI_PROFILE_PREFIX: &str = ".gemini-profile-";

/// Every numbered profile under `user_home`, in order.  Profile 0 is
/// not a profile: that number stands for the default home in a handoff
/// row.
pub(super) fn profile_dirs_in(user_home: &Path) -> Vec<(u8, PathBuf)> {
    let mut out: Vec<(u8, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(user_home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(GEMINI_PROFILE_PREFIX)) else {
                continue;
            };
            let Ok(n) = rest.parse::<u8>() else { continue };
            if n > 0 && e.path().is_dir() {
                out.push((n, e.path()));
            }
        }
    }
    out.sort_by_key(|(n, _)| *n);
    out
}

pub(crate) fn profile_dirs() -> Vec<(u8, PathBuf)> {
    std::env::var_os("HOME").map(|h| profile_dirs_in(Path::new(&h))).unwrap_or_default()
}

/// The homes a conversation can be handed to: every numbered profile,
/// or the default home as number 0 when there is none.  The default
/// counts only once gemini has made its directory there, so a machine
/// that never ran gemini is offered nothing.
pub(super) fn handoff_homes_in(user_home: &Path) -> Vec<(u8, PathBuf)> {
    let dirs = profile_dirs_in(user_home);
    if dirs.is_empty() && config_dir(user_home).is_dir() {
        return vec![(0, user_home.to_path_buf())];
    }
    dirs
}

pub(crate) fn handoff_homes() -> Vec<(u8, PathBuf)> {
    std::env::var_os("HOME").map(|h| handoff_homes_in(Path::new(&h))).unwrap_or_default()
}

/// Which profile a home is: `None` for the default one.  Resolved
/// through symlinks, so a home that links to `.gemini-profile-1` reads
/// as profile 1.
pub(super) fn profile_of(root: &Path) -> Option<u8> {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let n: u8 = real.file_name()?.to_str()?.strip_prefix(GEMINI_PROFILE_PREFIX)?.parse().ok()?;
    (n > 0).then_some(n)
}

/// `gemini` under the home `root`.  The variable is left off for the
/// user's own home, where it would only restate the default and then
/// be inherited by everything gemini runs.
pub(super) fn launch_in(root: &Path, user_home: &Path) -> Option<pty_op::PtyCommand> {
    let cmd = pty_op::PtyCommand::new("gemini");
    if root == user_home {
        return Some(cmd);
    }
    Some(cmd.env("GEMINI_CLI_HOME", root.to_str()?))
}

pub(crate) fn launch(root: &Path) -> Option<pty_op::PtyCommand> {
    launch_in(root, Path::new(&std::env::var_os("HOME")?))
}

/// What the pane is running, remembered so a menu pick knows what it
/// is changing and what to leave alone.
#[derive(Clone, Debug)]
pub(crate) struct PaneGemini {
    /// The gemini doing the work, which is the one to stop.  gemini
    /// normally relaunches itself as a child with a larger heap and the
    /// parent only waits for it; without a relaunch this is the process
    /// nearest the shell itself.
    pub worker: pidtree::ProcId,
    pub shell_pid: i32,
    pub root: PathBuf,
    pub profile: Option<u8>,
    pub cwd: Option<String>,
    /// The session this process is writing, when exactly one could be
    /// named.  `None` leaves the pane with nothing to resume or hand
    /// over, and both then do nothing.
    pub session: Option<String>,
}

/// Take gemini down and bring the SAME session back under another
/// account profile.
///
/// By id, never `--resume latest`: sessions are kept per project, and
/// two panes in one directory each have their own.  The caller has
/// checked that the target home holds this session — a home with its
/// own session store does not, and gemini would exit on the resume
/// after the running one was already gone.
///
/// From gemini's own directory, because that is the project gemini
/// looks the session up under, and the pane's shell may have moved.
///
/// The worker is the one signalled.  The waiting parent follows its
/// child down whichever signal ends it, while a parent killed after the
/// escalation would leave the child running on the pane's terminal.
pub(super) fn profile_switch_op(
    pane: &PaneGemini,
    profile: u8,
    root: &Path,
    session: &str,
    user_home: &Path,
) -> Option<pty_op::PtyOp> {
    let line = launch_in(root, user_home)?
        .arg("--resume")
        .arg(session)
        .arg("-i")
        .quoted_arg(crate::plugins::handoff::resume_word())
        .in_dir(pane.cwd.clone())
        .clear_screen_first(true)
        .to_bytes()?;
    Some(
        pty_op::PtyOp::new("gemini.profile_switch")
            .hold_screen(true)
            .badge(format!("→ P{profile}"))
            .step(pty_op::Step::settle(Duration::from_millis(250)).named("hold_settle"))
            .step(
                pty_op::Step::terminate(pane.worker, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(pty_op::Step::send(line).named("resume"))
            .step(
                pty_op::Step::await_process(pane.shell_pid, looks_like_gemini)
                    .timeout(Duration::from_secs(20)),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    /// The relaunched child's argv, as a running standalone build shows
    /// it: the executable path again, then the flags.
    #[test]
    fn gemini_argv_names_the_session_and_the_model() {
        let child = argv(&["/opt/gem/bin/gemini", "/opt/gem/bin/gemini", "-m", "flash", "--session-id", ID]);
        assert_eq!(
            argv_facts(&child),
            ArgvFacts { session: Some(ID.into()), model: Some("flash".into()) }
        );
        let resumed = argv(&["gemini", &format!("--resume={ID}"), "--model=gemini-3-pro"]);
        assert_eq!(
            argv_facts(&resumed),
            ArgvFacts { session: Some(ID.into()), model: Some("gemini-3-pro".into()) }
        );
        assert_eq!(argv_facts(&argv(&["gemini", "-r", ID])).session.as_deref(), Some(ID));
    }

    /// `latest` and an index pick a session only gemini can resolve.
    #[test]
    fn a_resume_that_names_no_session_binds_nothing() {
        for value in ["latest", "3", "-m", "x; rm -rf ~"] {
            assert_eq!(argv_facts(&argv(&["gemini", "--resume", value])).session, None, "{value}");
        }
        assert_eq!(argv_facts(&argv(&["gemini", "--resume"])), ArgvFacts::default());
        assert_eq!(argv_facts(&argv(&["gemini", "--session-id", "a b"])).session, None);
        assert_eq!(argv_facts(&argv(&["gemini", "-m", "two words"])).model, None);
    }

    #[test]
    fn the_home_is_the_variable_or_the_users_own() {
        let user = Path::new("/Users/x");
        assert_eq!(root_from(Some("/Users/x/.gemini-profile-2".into()), user), Path::new("/Users/x/.gemini-profile-2"));
        assert_eq!(root_from(None, user), user);
        assert_eq!(root_from(Some(String::new()), user), user, "gemini reads an empty value as unset");
        assert_eq!(config_dir(Path::new("/Users/x/.gemini-profile-2")), Path::new("/Users/x/.gemini-profile-2/.gemini"));
    }

    #[test]
    fn a_numbered_home_is_a_profile_and_the_default_is_not() {
        let home = tmp("gemini-profiles");
        for name in [".gemini-profile-2", ".gemini-profile-1", ".gemini-profile-0", ".gemini-profile-x", ".gemini"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }
        std::fs::write(home.join(".gemini-profile-7"), "a file, not a home").unwrap();
        assert_eq!(
            profile_dirs_in(&home),
            vec![(1, home.join(".gemini-profile-1")), (2, home.join(".gemini-profile-2"))]
        );
        assert_eq!(profile_of(&home.join(".gemini-profile-2")), Some(2));
        assert_eq!(profile_of(&home), None, "the default home has no number");
        std::os::unix::fs::symlink(home.join(".gemini-profile-1"), home.join("linked")).unwrap();
        assert_eq!(profile_of(&home.join("linked")), Some(1), "a link reads as what it points at");
    }

    #[test]
    fn a_handoff_goes_to_the_default_home_only_when_there_is_no_profile() {
        let home = tmp("gemini-handoff-homes");
        assert_eq!(handoff_homes_in(&home), Vec::new(), "gemini never ran here");
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        assert_eq!(handoff_homes_in(&home), vec![(0, home.clone())]);
        std::fs::create_dir_all(home.join(".gemini-profile-3")).unwrap();
        assert_eq!(handoff_homes_in(&home), vec![(3, home.join(".gemini-profile-3"))]);
    }

    fn pane() -> PaneGemini {
        PaneGemini {
            worker: pidtree::ProcId { pid: 4250, start_us: 2 },
            shell_pid: 4200,
            root: PathBuf::from("/Users/x/.gemini-profile-1"),
            profile: Some(1),
            cwd: Some("/w/a".into()),
            session: Some(ID.into()),
        }
    }

    fn line_of(op: &pty_op::PtyOp) -> String {
        op.steps
            .iter()
            .filter_map(|s| match &s.kind {
                pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_switch_resumes_the_same_session_under_the_other_home() {
        let user = Path::new("/Users/x");
        let op = profile_switch_op(&pane(), 2, Path::new("/Users/x/.gemini-profile-2"), ID, user).expect("op builds");
        let word = crate::plugins::handoff::resume_word();
        assert_eq!(
            line_of(&op),
            format!(
                "printf '\\033[H\\033[2J'; ( cd '/w/a' && GEMINI_CLI_HOME='/Users/x/.gemini-profile-2' \
                 exec gemini --resume {ID} -i '{word}' )\r"
            )
        );
        assert_eq!(op.badge.as_deref(), Some("→ P2"));
        assert!(
            op.steps.iter().any(|s| matches!(
                &s.kind,
                pty_op::StepKind::Terminate { target, .. } if target.pid == 4250
            )),
            "the worker is signalled, not typed at and not its waiting parent"
        );
    }

    #[test]
    fn the_default_home_needs_no_variable() {
        let user = Path::new("/Users/x");
        let line = launch_in(user, user).unwrap().to_bytes().unwrap();
        assert_eq!(String::from_utf8(line).unwrap(), "gemini\r");
    }

    #[test]
    fn a_home_that_cannot_be_quoted_gets_no_switch() {
        let user = Path::new("/Users/x");
        assert!(profile_switch_op(&pane(), 2, Path::new("/Users/x/it's"), ID, user).is_none());
        assert!(profile_switch_op(&pane(), 2, Path::new("/Users/x/.gemini-profile-2"), "x; rm -rf ~", user).is_none());
    }
}

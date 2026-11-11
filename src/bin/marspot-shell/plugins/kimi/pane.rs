//! The kimi process behind a pane: which home it runs under, which
//! account profile that is, and how one is started under another.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use crate::plugins::pty_op;

/// The directory kimi keeps everything in: its `KIMI_CODE_HOME`, or
/// `~/.kimi-code`.  The variable names the directory itself, as
/// `CODEX_HOME` does.
pub(super) fn home_from(kimi_code_home: Option<String>, user_home: &Path) -> PathBuf {
    match kimi_code_home {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => default_home(user_home),
    }
}

pub(crate) fn default_home(user_home: &Path) -> PathBuf {
    user_home.join(".kimi-code")
}

/// Account profiles: `~/.kimi-code-profile-N` is a whole kimi home
/// with its own login, settings and sessions, the convention the other
/// agents' profiles follow.
const KIMI_PROFILE_PREFIX: &str = ".kimi-code-profile-";

/// Every numbered profile under `user_home`, in order.  Profile 0 is
/// not a profile: that number stands for the default home.
pub(crate) fn profile_dirs_in(user_home: &Path) -> Vec<(u8, PathBuf)> {
    let mut out: Vec<(u8, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(user_home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(KIMI_PROFILE_PREFIX)) else {
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

/// The homes a conversation can be handed to: every numbered profile,
/// or the default home as number 0 when there is none.  The default
/// counts only once it exists, so a machine that never had kimi is
/// offered nothing.
pub(super) fn handoff_homes_in(user_home: &Path) -> Vec<(u8, PathBuf)> {
    let dirs = profile_dirs_in(user_home);
    if dirs.is_empty() && default_home(user_home).is_dir() {
        return vec![(0, default_home(user_home))];
    }
    dirs
}

pub(super) fn profile_dirs() -> Vec<(u8, PathBuf)> {
    std::env::var_os("HOME").map(|h| profile_dirs_in(Path::new(&h))).unwrap_or_default()
}

pub(crate) fn handoff_homes() -> Vec<(u8, PathBuf)> {
    std::env::var_os("HOME").map(|h| handoff_homes_in(Path::new(&h))).unwrap_or_default()
}

thread_local! {
    /// The model alias the next kimi started from this thread is given.
    static MODEL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Run `f` with every kimi it starts given `-m <alias>`: how a pane
/// that moves to another kimi account stays on the model it was on.
pub(super) fn with_model<T>(alias: Option<String>, f: impl FnOnce() -> T) -> T {
    MODEL.with(|m| *m.borrow_mut() = alias);
    let out = f();
    MODEL.with(|m| *m.borrow_mut() = None);
    out
}

/// `kimi` under `home`, on session `resume` or on a new one.
///
/// Always with the workspace trusted for this run: the question kimi
/// asks about a directory it has not seen would otherwise be what the
/// handoff is typed into.  A resumed session restores its own model,
/// so only a new one is told which to use.  There is no flag for the
/// thinking effort; a new session takes the home's configured one.
pub(super) fn launch_in(home: &Path, user_home: &Path, resume: Option<&str>) -> Option<pty_op::PtyCommand> {
    let mut cmd = pty_op::PtyCommand::new("kimi").env("KIMI_CODE_TRUST_WORKSPACE", "1");
    if home != default_home(user_home) {
        cmd = cmd.env("KIMI_CODE_HOME", home.to_str()?);
    }
    match (resume, MODEL.with(|m| m.borrow().clone())) {
        (Some(id), _) => Some(cmd.arg("--session").arg(id)),
        (None, Some(alias)) if super::config::plain(&alias) => Some(cmd.arg("-m").arg(alias)),
        (None, _) => Some(cmd),
    }
}

pub(crate) fn launch(home: &Path, resume: Option<&str>) -> Option<pty_op::PtyCommand> {
    launch_in(home, Path::new(&std::env::var_os("HOME")?), resume)
}

/// Which profile a home is: `None` for the default one.  Resolved
/// through symlinks, so a home that links to a profile reads as it.
pub(super) fn profile_of(home: &Path) -> Option<u8> {
    let real = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let n: u8 = real.file_name()?.to_str()?.strip_prefix(KIMI_PROFILE_PREFIX)?.parse().ok()?;
    (n > 0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    #[test]
    fn the_home_is_the_variable_or_the_default_directory() {
        let user = Path::new("/Users/x");
        assert_eq!(home_from(Some("/Users/x/.kimi-code-profile-2".into()), user), Path::new("/Users/x/.kimi-code-profile-2"));
        assert_eq!(home_from(None, user), Path::new("/Users/x/.kimi-code"));
        assert_eq!(home_from(Some(String::new()), user), Path::new("/Users/x/.kimi-code"));
    }

    #[test]
    fn a_handoff_goes_to_the_default_home_only_when_there_is_no_profile() {
        let home = tmp("kimi-handoff-homes");
        assert_eq!(handoff_homes_in(&home), Vec::new(), "kimi never ran here");
        std::fs::create_dir_all(home.join(".kimi-code")).unwrap();
        assert_eq!(handoff_homes_in(&home), vec![(0, home.join(".kimi-code"))]);
        std::fs::create_dir_all(home.join(".kimi-code-profile-3")).unwrap();
        assert_eq!(handoff_homes_in(&home), vec![(3, home.join(".kimi-code-profile-3"))]);
    }

    #[test]
    fn a_new_session_is_told_the_model_and_a_resumed_one_is_not() {
        let user = Path::new("/Users/x");
        let line = |home: &str, resume: Option<&str>| String::from_utf8(launch_in(Path::new(home), user, resume).unwrap().to_bytes().unwrap()).unwrap();
        let plain = line("/Users/x/.kimi-code", None);
        assert!(plain.ends_with(" kimi\r") && !plain.contains("KIMI_CODE_HOME"), "{plain}");
        assert!(plain.contains("KIMI_CODE_TRUST_WORKSPACE="), "{plain}");
        let other = line("/Users/x/.kimi-code-profile-2", Some("session_1"));
        assert!(other.contains("KIMI_CODE_HOME='/Users/x/.kimi-code-profile-2'") && other.ends_with(" kimi --session session_1\r"), "{other}");
        with_model(Some("kimi-code/k3".into()), || {
            assert!(line("/Users/x/.kimi-code", None).ends_with(" kimi -m kimi-code/k3\r"));
            assert!(line("/Users/x/.kimi-code", Some("session_1")).ends_with(" kimi --session session_1\r"));
        });
        assert!(line("/Users/x/.kimi-code", None).ends_with(" kimi\r"), "the model is carried for one switch only");
        with_model(Some("x; rm -rf ~".into()), || assert!(line("/Users/x/.kimi-code", None).ends_with(" kimi\r")));
        assert!(launch_in(Path::new("/Users/x/it's"), user, None).unwrap().to_bytes().is_none(), "a home that cannot be quoted");
    }

    #[test]
    fn a_numbered_home_is_a_profile_and_the_default_is_not() {
        let home = tmp("kimi-profiles");
        for name in [".kimi-code-profile-2", ".kimi-code-profile-0", ".kimi-code"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }
        std::fs::write(home.join(".kimi-code-profile-7"), "a file, not a home").unwrap();
        assert_eq!(profile_dirs_in(&home), vec![(2, home.join(".kimi-code-profile-2"))]);
        assert_eq!(profile_of(&home.join(".kimi-code-profile-2")), Some(2));
        assert_eq!(profile_of(&home.join(".kimi-code-profile-0")), None, "0 stands for the default home");
        assert_eq!(profile_of(&home.join(".kimi-code")), None);
        std::os::unix::fs::symlink(home.join(".kimi-code-profile-2"), home.join("linked")).unwrap();
        assert_eq!(profile_of(&home.join("linked")), Some(2), "a link reads as what it points at");
    }
}

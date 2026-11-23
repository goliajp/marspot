//! Which directories a container may have as its working directory, and
//! what of a git repository in it is mounted read-only.

use std::path::{Path, PathBuf};

use marspot_peer::control::{AgentKind, ErrorCode, Reject};

use super::super::launch::agent_of;
use crate::plugins::handoff;

/// How the names of the agents' directories in the user's home begin:
/// each account's, the default one, the one they share.
const AGENT_DIRS: [&str; 5] = [".claude", ".codex", ".gemini", ".kimi", ".agents"];
/// The volume behind `/Users`: every path there has a second name under it.
const DATA: &str = "/System/Volumes/Data";

/// `p` by one name, through links and the data volume.
fn real(p: &Path) -> PathBuf {
    let r = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    r.strip_prefix(DATA).map(|rest| Path::new("/").join(rest)).unwrap_or(r)
}

/// Whether an entry of the user's home is where programs keep their
/// own files.
fn private(name: &str) -> bool {
    name.starts_with('.') || name == "Library"
}

/// The directories whose files the host runs or trusts.
fn protected(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut out = vec![("marspot's state directory", real(&marspot_term::paths::state_root()))];
    for e in std::fs::read_dir(home).into_iter().flatten().flatten() {
        if AGENT_DIRS.iter().any(|d| e.file_name().to_string_lossy().starts_with(d)) {
            out.push(("an agent's directory", real(&e.path())));
        }
    }
    for k in [AgentKind::Claude, AgentKind::Codex, AgentKind::Gemini, AgentKind::Kimi] {
        // gemini's root may be the user's own home, which is looked at apart
        let homes = handoff::profiles(agent_of(k)).into_iter().map(|(_, p)| real(&p)).filter(|p| p != home);
        out.extend(homes.map(|p| ("an agent's directory", p)));
    }
    out
}

/// The real path of `cwd`, when a container may have it.
pub(super) fn cwd(cwd: &Path) -> Result<PathBuf, Reject> {
    let bad = |m: String| Reject::new(ErrorCode::BadRequest, m);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(bad(format!("{} is not the full path of a directory", cwd.display())));
    }
    let at = real(cwd);
    let home = real(&std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default());
    if home.starts_with(&at) || Path::new(DATA).starts_with(&at) {
        return Err(bad(format!("{} is or holds your home directory, which a container may not have; give a project's directory", at.display())));
    }
    // a link among them has its directory elsewhere
    let hidden = std::fs::read_dir(&home).into_iter().flatten().flatten().filter(|e| private(&e.file_name().to_string_lossy())).map(|e| real(&e.path()));
    if let Some(p) = hidden.filter(|p| p.is_dir()).find(|p| at.starts_with(p)) {
        return Err(bad(format!("{} is or is inside {}, which holds settings and logins of programs of the host; give a project's directory", at.display(), p.display())));
    }
    for (what, p) in protected(&home) {
        if at.starts_with(&p) || p.starts_with(&at) {
            return Err(bad(format!("{} is, holds or is inside {what} ({}), which a container may not have", at.display(), p.display())));
        }
    }
    Ok(at)
}

/// What is mounted read-only over the working directory `cwd`, named
/// from it: the file and the directory by which git runs commands; of a
/// worktree, the file that says where its repository is.
pub(super) fn git_parts(cwd: &Path) -> Vec<&'static str> {
    let kind = |p: &str| std::fs::symlink_metadata(cwd.join(p)).ok().map(|m| m.file_type());
    match kind(".git") {
        Some(t) if t.is_file() => vec![".git"],
        Some(t) if t.is_dir() => {
            let parts = [(".git/config", false), (".git/hooks", true)];
            parts.into_iter().filter(|(p, dir)| kind(p).is_some_and(|t| if *dir { t.is_dir() } else { t.is_file() })).map(|(p, _)| p).collect()
        }
        _ => Vec::new(),
    }
}

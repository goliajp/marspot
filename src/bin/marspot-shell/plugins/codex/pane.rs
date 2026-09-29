//! The codex process behind a pane: which profile directory it runs
//! under, and how to move it to another one.

use std::path::PathBuf;
use std::time::Duration;

use super::looks_like_codex;
use crate::plugins::{pidtree, pty_op};

/// The thread a codex process was started to resume, from its argv
/// (`codex resume <uuid>`).  None for a fresh one, or `--last`.
pub(crate) fn argv_thread_id(codex_pid: i32) -> Option<String> {
    let line = pidtree::proc_cmdline(codex_pid)?;
    let mut it = line.split(' ');
    it.by_ref().find(|w| *w == "resume")?;
    it.find(|w| !w.starts_with('-'))
        .filter(|w| w.len() == 36 && w.chars().filter(|c| *c == '-').count() == 4)
        .map(str::to_string)
}

/// The home a live codex runs under: its `CODEX_HOME`, or `~/.codex`.
pub(crate) fn home_of(codex_pid: i32) -> Option<PathBuf> {
    marspot::pidtree::proc_env_value(codex_pid, "CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// The reasoning efforts codex accepts.
///
/// Read out of the binary rather than assumed: its serde variant table
/// carries `low`/`medium`/`high` adjacently, and the only `minimal` in
/// there belongs to filesystem paths.
/// Account profiles, the `CODEX_HOME` kind.
///
/// Not `codex -p <name>`, which layers a named set of CONFIG values
/// (`$CODEX_HOME/<name>.config.toml`).  This is the other axis: a
/// whole directory with its own login, history and config, selected by
/// pointing `CODEX_HOME` at it — the exact analogue of
/// `CLAUDE_CONFIG_DIR`, which the claudecode plugin next door has
/// cycled for a year.
///
/// `~/.codex` itself is the default, and on this machine it is a
/// SYMLINK to `.codex-profile-1`; a link is followed so the default and
/// the profile it points at are not offered as two separate things
/// that do the same.
const CODEX_PROFILE_PREFIX: &str = ".codex-profile-";

/// Which profile a directory is: `None` for the default `~/.codex`.
pub(crate) fn profile_dirs() -> Vec<(u8, std::path::PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    let mut out: Vec<(u8, std::path::PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix(CODEX_PROFILE_PREFIX) else {
                continue;
            };
            let Ok(n) = rest.parse::<u8>() else { continue };
            if e.path().is_dir() {
                out.push((n, e.path()));
            }
        }
    }
    out.sort_by_key(|(n, _)| *n);
    out
}

/// The profile a live codex belongs to, read off the process rather
/// than reconstructed.
///
/// `CODEX_HOME` is what the `codexN` shell aliases set, and reading it
/// back is the alias's own expansion made explicit — reproducing the
/// alias would depend on the user's rc file still defining it, in that
/// shell, at that moment.  Unset means the default `~/.codex`, which
/// is resolved through symlinks so a link to `.codex-profile-1` reads
/// as profile 1 and not as a nameless "default".
pub(super) fn profile_of(codex_pid: i32) -> Option<u8> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let raw = marspot::pidtree::proc_env_value(codex_pid, "CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let real = std::fs::canonicalize(&raw).unwrap_or(raw);
    let name = real.file_name()?.to_str()?;
    name.strip_prefix(CODEX_PROFILE_PREFIX)?.parse().ok()
}


/// What the pane is running, remembered so a menu pick knows what it
/// is changing and what to leave alone.
#[derive(Clone)]
pub(crate) struct PaneCodex {
    pub codex_pid: i32,
    pub shell_pid: i32,
    pub effort: Option<String>,
    /// Which `CODEX_HOME` this pane's codex is running under, read off
    /// the live process.  `None` when it could not be determined —
    /// the menu then marks nothing as current rather than guessing.
    pub profile: Option<u8>,
}


/// Take codex down and bring the SAME session back under another
/// account profile.
///
/// `CODEX_HOME` on the resume line rather than the `codexN` alias: an
/// alias is an interactive shell's, defined in the user's rc file, and
/// reproducing one depends on all of that still being true in that
/// shell at that moment.  Setting the variable IS what the alias
/// expands to.
///
/// `resume --last` and not a session id, for the same reason the
/// effort switch uses it: codex filters the picker by working
/// directory, so within a pane's own cwd the most recent session is
/// that pane's.  NOTE that history lives inside the profile directory,
/// so resuming under a DIFFERENT profile finds that profile's most
/// recent session in this cwd — which is the honest meaning of
/// switching accounts, not a bug to paper over.
pub(super) fn profile_switch_op(pane: &PaneCodex, profile: u8, dir: &std::path::Path) -> Option<pty_op::PtyOp> {
    let dir = dir.to_str()?;
    let line = pty_op::PtyCommand::new("codex")
        .env("CODEX_HOME", dir)
        .arg("resume")
        .arg("--last")
        .clear_screen_first(true)
        .to_bytes()?;
    Some(
        pty_op::PtyOp::new("codex.profile_switch")
            .hold_screen(true)
            .badge(format!("→ P{profile}"))
            .step(pty_op::Step::settle(Duration::from_millis(250)).named("hold_settle"))
            .step(
                pty_op::Step::terminate(pane.codex_pid, libc::SIGTERM)
                    .escalate_after(Duration::from_secs(3), libc::SIGKILL),
            )
            .step(pty_op::Step::send(line).named("resume"))
            .step(
                pty_op::Step::await_process(pane.shell_pid, looks_like_codex)
                    .timeout(Duration::from_secs(20)),
            ),
    )
}

#[cfg(test)]
mod profile_menu_tests {
    use super::{profile_switch_op, PaneCodex, CODEX_PROFILE_PREFIX};
    use std::path::Path;

    fn pane(profile: u8) -> PaneCodex {
        PaneCodex {
            codex_pid: 4242,
            shell_pid: 4200,
            effort: Some("medium".into()),
            profile: Some(profile),
        }
    }

    fn line_of(op: &super::pty_op::PtyOp) -> String {
        op.steps
            .iter()
            .filter_map(|s| match &s.kind {
                super::pty_op::StepKind::Send(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_switch_sets_the_variable_the_alias_expands_to() {
        // `codexN` is an interactive shell alias; reproducing it would
        // depend on the user's rc file still defining it, in that
        // shell, at that moment.  What it expands to is a variable,
        // and that is what goes on the line.
        let op = profile_switch_op(&pane(1), 2, Path::new("/Users/x/.codex-profile-2"))
            .expect("op builds");
        let line = line_of(&op);
        assert!(
            line.contains("CODEX_HOME='/Users/x/.codex-profile-2'"),
            "{line}"
        );
        assert!(line.contains("codex resume --last"), "{line}");
        // The pane's own screen is cleared first so the new session
        // does not paint over the old one's tail.  It goes out as a
        // shell `printf`, not a raw escape — the line is typed at a
        // shell, so the shell is what emits it.
        assert!(line.contains(r"printf '\033[H\033[2J'"), "{line}");
    }

    #[test]
    fn codex_is_signalled_not_typed_at() {
        // A `/quit` typed through the PTY echoes into the grid; a
        // signal does not.  Same rule the claudecode plugin follows.
        let op = profile_switch_op(&pane(1), 2, Path::new("/Users/x/.codex-profile-2"))
            .expect("op builds");
        assert!(
            op.steps.iter().any(|s| matches!(
                &s.kind,
                super::pty_op::StepKind::Terminate { pid, .. } if *pid == 4242
            )),
            "the running codex must be signalled"
        );
    }

    #[test]
    fn the_prefix_is_the_one_the_directories_use() {
        // The discovery scan and the alias the user types have to agree
        // on the name, and there is nothing else pinning that.
        assert_eq!(CODEX_PROFILE_PREFIX, ".codex-profile-");
    }
}

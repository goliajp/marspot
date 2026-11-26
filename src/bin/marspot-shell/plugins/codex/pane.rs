//! The codex process behind a pane: which profile directory it runs
//! under, and how to move it to another one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::looks_like_codex;
use crate::plugins::{pidtree, pty_op};

/// The thread a codex process was started to resume, from its argv
/// (`codex resume <uuid>`).  None for a fresh one, or `--last`.
pub(crate) fn argv_thread_id(codex_pid: i32) -> Option<String> {
    let line = pidtree::proc_cmdline(codex_pid)?;
    let mut it = line.split(' ');
    it.by_ref().find(|w| *w == "resume")?;
    it.find(|w| !w.starts_with('-')).filter(|w| is_thread_id(w)).map(str::to_string)
}

/// Shaped like a thread id: a uuid.
pub(super) fn is_thread_id(w: &str) -> bool {
    w.len() == 36 && w.chars().filter(|c| *c == '-').count() == 4
}

/// The home a live codex runs under: its `CODEX_HOME`, or `~/.codex`.
pub(crate) fn home_of(codex_pid: i32) -> Option<PathBuf> {
    home_from(marspot::pidtree::proc_env_value(codex_pid, "CODEX_HOME"))
}

/// [`home_of`] from the process's `CODEX_HOME`, already read.
pub(super) fn home_from(codex_home: Option<String>) -> Option<PathBuf> {
    codex_home
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
///
/// Takes the process's `CODEX_HOME`, already read.
pub(super) fn profile_from(codex_home: Option<String>) -> Option<u8> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let raw = codex_home
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
    /// From an observation up to two seconds old; the switch's
    /// terminate step re-checks it when it acts.
    pub codex: pidtree::ProcId,
    pub shell_pid: i32,
    /// Which `CODEX_HOME` this pane's codex is running under, read off
    /// the live process.  `None` when it could not be determined —
    /// the menu then marks nothing as current rather than guessing.
    pub profile: Option<u8>,
    /// Its command line, when it could be read.
    pub argv: Option<Vec<String>>,
}

/// Take codex down and bring the SAME thread back under another
/// account profile, in the directory it ran in.
///
/// `CODEX_HOME` on the resume line rather than the `codexN` alias: an
/// alias is an interactive shell's, defined in the user's rc file, and
/// reproducing one depends on all of that still being true in that
/// shell at that moment.  Setting the variable IS what the alias
/// expands to.  The thread by id: `--last` picks the newest thread a
/// home has registered for the directory, and a directory can hold two.
/// Nothing is started before `carry.release` says the old home has let go.
pub(super) fn profile_switch_op(pane: &PaneCodex, profile: u8, dir: &Path, thread: &str, cwd: Option<String>, carry: Carry) -> Option<pty_op::PtyOp> {
    resume_op("codex.profile_switch", format!("→ P{profile}"), pane, dir, thread, cwd, carry)
}

/// What a resume brings across besides the thread.
pub(super) struct Carry {
    /// The composer's unsent text: taken by this op, or by an earlier
    /// one that ended the TUI when `taken`.
    pub held: Arc<pty_op::Job>,
    pub taken: bool,
    /// A switch: called once the TUI is gone, then waited on until the
    /// old home has let go.
    pub release: Option<(pty_op::CallFn, Arc<pty_op::Job>)>,
    /// The model the thread goes on with (`-m`); None when nothing was
    /// ever observed, and the line then names none.
    pub model: Option<String>,
}

impl Carry {
    pub(super) fn fresh(model: Option<String>) -> Carry {
        Carry { held: pty_op::Job::new(), taken: false, release: None, model }
    }
}

/// Take an outdated codex TUI down and bring the same thread back
/// under the codex now installed (see `update`).
///
/// `codex` from the shell's PATH, which is where the installer puts the
/// current release.  The daemon keeps the thread and anything it is
/// running while no TUI is attached, so nothing but the screen is
/// redrawn.
pub(super) fn refresh_op(pane: &PaneCodex, home: &Path, thread: &str, cwd: Option<String>, to: super::update::Release) -> Option<pty_op::PtyOp> {
    let model = super::last_model::thread_model(home, thread);
    resume_op("codex.refresh", format!("↻ {to}"), pane, home, thread, cwd, Carry::fresh(model))
}

/// End the TUI and start `codex resume <thread>` under `home`, with
/// what was in the composer put back once the new one has drawn; in
/// between, wait for the old home's release when there is one.
pub(super) fn resume_op(name: &'static str, badge: String, pane: &PaneCodex, home: &Path, thread: &str, cwd: Option<String>, carry: Carry) -> Option<pty_op::PtyOp> {
    let mut cmd = pty_op::PtyCommand::new("codex").env("CODEX_HOME", home.to_str()?).arg("resume").arg(thread);
    if let Some(m) = &carry.model {
        cmd = cmd.arg("-m").arg(m.clone());
    }
    let line = cmd.in_dir(cwd).clear_screen_first(true).to_bytes()?;
    let mut op = pty_op::PtyOp::new(name).hold_screen(true).badge(badge);
    if !carry.taken {
        op = op.step(pty_op::Step::capture_composer(Arc::clone(&carry.held)));
    }
    op = op
        .step(pty_op::Step::settle(Duration::from_millis(250)).named("hold_settle"))
        .step(pty_op::Step::terminate(pane.codex, libc::SIGTERM).escalate_after(Duration::from_secs(3), libc::SIGKILL));
    if let Some((gone, job)) = carry.release {
        // the worker decides at its own deadline, counted from the call;
        // this one only backs it up
        let wait = crate::plugins::peerbus::moving::RELEASE_WAIT + Duration::from_secs(10);
        op = op.step(pty_op::Step::call(gone).named(RELEASE_CLOCK)).step(pty_op::Step::await_job(job).timeout(wait).named(RELEASED));
    }
    Some(
        op.shell_line(pane.shell_pid, line, "resume")
            .step(
                pty_op::Step::await_quiet(Duration::from_millis(1500))
                    .after_bytes(2048)
                    .timeout(Duration::from_secs(30))
                    .named("first_frame"),
            )
            .step(pty_op::Step::await_process(pane.shell_pid, looks_like_codex).timeout(Duration::from_secs(5)))
            .step(pty_op::Step::restore_composer(carry.held)),
    )
}

/// The steps of a switch that start and await the old home's release.
pub(super) const RELEASE_CLOCK: &str = "release_clock";
pub(super) const RELEASED: &str = "old_home_released";

#[cfg(test)]
mod profile_menu_tests {
    use super::{profile_switch_op, Carry, PaneCodex, CODEX_PROFILE_PREFIX};
    use std::path::Path;
    use std::sync::Arc;

    fn switching() -> Carry {
        Carry { release: Some((Arc::new(|| Ok(())), super::pty_op::Job::new())), ..Carry::fresh(Some("gpt-5.5".into())) }
    }

    fn pane(profile: u8) -> PaneCodex {
        PaneCodex {
            codex: marspot::pidtree::ProcId { pid: 4242, start_us: 0 },
            shell_pid: 4200,
            profile: Some(profile),
            argv: None,
        }
    }

    const THREAD: &str = "01a133f4-8e7b-7331-9630-9c40ff4b467f";

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
    fn the_switch_resumes_the_thread_by_id_under_the_variable_the_alias_expands_to() {
        // `codexN` is an interactive shell alias; reproducing it would
        // depend on the user's rc file still defining it, in that
        // shell, at that moment.  What it expands to is a variable,
        // and that is what goes on the line.
        let op = profile_switch_op(&pane(1), 2, Path::new("/Users/x/.codex-profile-2"), THREAD, Some("/Users/x/work".into()), switching())
            .expect("op builds");
        let line = line_of(&op);
        assert!(line.contains("CODEX_HOME='/Users/x/.codex-profile-2'"), "{line}");
        assert!(line.contains(&format!("codex resume {THREAD} -m gpt-5.5")), "the thread's model, not config.toml's: {line}");
        assert!(!line.contains("--last"), "a directory can hold two threads: {line}");
        assert!(line.contains("cd '/Users/x/work'"), "{line}");
        // The pane's own screen is cleared first so the new session
        // does not paint over the old one's tail.  It goes out as a
        // shell `printf`, not a raw escape — the line is typed at a
        // shell, so the shell is what emits it.
        assert!(line.contains(r"printf '\033[H\033[2J'"), "{line}");
        // what was typed and not sent comes back after the first frame
        let kinds: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
        let at = |l: &str| kinds.iter().position(|k| *k == l).unwrap_or_else(|| panic!("{l} in {kinds:?}"));
        assert!(at("capture_composer") < at("resume") && at("first_frame") < at("restore_composer"), "{kinds:?}");
        // the new home loads the thread only once the old one has let go
        // and its wait starts once the TUI is gone
        assert!(at("terminate") < at(super::RELEASE_CLOCK) && at(super::RELEASE_CLOCK) < at(super::RELEASED), "{kinds:?}");
        assert!(at(super::RELEASED) < at("resume"), "{kinds:?}");
        assert_eq!(op.badge.as_deref(), Some("→ P2"));
    }

    #[test]
    fn codex_is_signalled_not_typed_at() {
        // A `/quit` typed through the PTY echoes into the grid; a
        // signal does not.  Same rule the claudecode plugin follows.
        let op = profile_switch_op(&pane(1), 2, Path::new("/Users/x/.codex-profile-2"), THREAD, None, switching()).expect("op builds");
        assert!(
            op.steps.iter().any(|s| matches!(
                &s.kind,
                super::pty_op::StepKind::Terminate { target, .. } if target.pid == 4242
            )),
            "the running codex must be signalled"
        );
    }

    #[test]
    fn a_refresh_resumes_the_same_thread_by_id_and_keeps_the_draft() {
        let release = super::super::update::Release::parse("0.164.0").unwrap();
        let op = super::refresh_op(
            &pane(1),
            Path::new("/Users/x/.codex-profile-1"),
            THREAD,
            Some("/Users/x/work/alt".into()),
            release,
        )
        .expect("op builds");
        let line = line_of(&op);
        assert!(line.contains("CODEX_HOME='/Users/x/.codex-profile-1'"), "{line}");
        assert!(line.contains("codex resume 01a133f4-8e7b-7331-9630-9c40ff4b467f"), "{line}");
        assert!(!line.contains("--last"), "{line}");
        assert!(line.contains("cd '/Users/x/work/alt'"), "{line}");
        let kinds: Vec<&str> = op.steps.iter().map(|s| s.label).collect();
        let at = |l: &str| kinds.iter().position(|k| *k == l).unwrap_or_else(|| panic!("{l} in {kinds:?}"));
        assert!(at("capture_composer") < at("resume"), "{kinds:?}");
        assert_eq!(kinds.last(), Some(&"restore_composer"), "{kinds:?}");
        assert_eq!(op.badge.as_deref(), Some("↻ 0.164.0"));
    }

    #[test]
    fn a_thread_that_cannot_go_on_a_command_line_gets_no_refresh() {
        let release = super::super::update::Release::parse("0.164.0").unwrap();
        let op = super::refresh_op(&pane(1), Path::new("/Users/x/.codex-profile-1"), "x; rm -rf ~", None, release);
        assert!(op.is_none());
    }

    #[test]
    fn the_prefix_is_the_one_the_directories_use() {
        // The discovery scan and the alias the user types have to agree
        // on the name, and there is nothing else pinning that.
        assert_eq!(CODEX_PROFILE_PREFIX, ".codex-profile-");
    }
}

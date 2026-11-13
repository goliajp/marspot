//! The shell's side of `marspot_term::owner`: saying who answers for
//! the sessions, and getting into their coalition when it is not ours.
//!
//! A shell the user starts is born in a coalition of its own.  Sessions
//! it then starts are born there too, and it answers for them until the
//! last one ends (`resident`).  The sessions it finds already running
//! are another matter: a shell that died left them in its coalition,
//! and no process outside a coalition can answer for it.  A session
//! normally starts a successor there the moment the shell dies, so this
//! shell never gets to run.  When it does run and finds every session
//! elsewhere, it asks for an heir in their coalition and leaves; the
//! heir is the app from then on.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use marspot::{lx_event, lx_warn};
use marspot_term::owner::{self, Record};
use marspot_term::{platform, session_registry};

/// How long a shell outside the sessions' coalition waits for an heir
/// in it before it takes the sessions as they are.
const HEIR_WAIT: Duration = Duration::from_secs(3);

fn own_coalition() -> Option<u64> {
    platform::coalition_of(std::process::id() as i32)
}

/// What a session starts to get another shell: the bundle's main
/// executable, which is what this process was started as.
/// LaunchServices takes a process for the app by the file it was
/// started from: opening the app reaches a shell started from the
/// bundle, and starts a second app beside one started from a copy
/// outside it, which leaves the sessions' coalition to whoever
/// registers next.  An image this process moved to since (an update)
/// keeps the directory it came from in `MARSPOT_BUNDLE_DIR`.
pub(crate) fn successor_exe() -> PathBuf {
    match std::env::var_os("MARSPOT_BUNDLE_DIR") {
        Some(dir) => PathBuf::from(dir).join("marspot-shell"),
        None => std::env::current_exe().unwrap_or_default(),
    }
}

/// The app bundle `exe` is the main executable of, if it is in one.
pub(crate) fn bundle_of(exe: &std::path::Path) -> Option<&std::path::Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.ends_with("MacOS") && contents.ends_with("Contents") && bundle.extension().is_some_and(|e| e == "app"))
        .then_some(bundle)
}

fn record(heir_in: Option<u64>, heir_resident: bool) -> Record {
    Record {
        pid: std::process::id() as i32,
        coalition: own_coalition(),
        exe: successor_exe(),
        heir_in,
        heir_resident,
    }
}

/// Put this process on record as the one that answers.
pub(crate) fn announce() {
    if let Err(e) = record(None, false).write() {
        lx_warn!("shell.owner.record_failed", &format!("{e}"));
    }
}

struct Kept {
    sid: u64,
    coalition: Option<u64>,
    /// Whether it can start a shell when asked.
    guards: bool,
}

fn kept_sessions() -> Vec<Kept> {
    session_registry::list_session_entries()
        .into_iter()
        .filter(|e| session_registry::pid_is_live_session(e.pid))
        .map(|e| Kept {
            sid: e.id,
            coalition: platform::coalition_of(e.pid),
            guards: session_registry::session_dir(e.id).join("guard").exists(),
        })
        .collect()
}

/// One line per running session: its coalition, and this process when
/// it is the one that answers for it.  A session kept from before the
/// app could stay resident is in a coalition nobody answers for, and
/// says `none` until it ends.
pub(crate) fn log_sessions() {
    let mine = own_coalition();
    for s in kept_sessions() {
        let ours = mine.is_some() && s.coalition == mine;
        lx_event!(
            "SESSION_OWNER",
            "who answers for this session's coalition",
            sid = s.sid,
            coalition = s.coalition.map_or_else(|| "unknown".to_string(), |c| c.to_string()),
            owner_pid = if ours { std::process::id().to_string() } else { "none".to_string() }
        );
    }
}

/// The coalition to move to: where the sessions are when none is with
/// us, and the one with the most sessions able to start a shell.
fn coalition_to_join(mine: Option<u64>, sessions: &[(Option<u64>, bool)]) -> Option<u64> {
    if mine.is_none() || sessions.iter().any(|(c, _)| *c == mine) {
        return None;
    }
    let mut counts: Vec<(u64, usize)> = Vec::new();
    for c in sessions.iter().filter(|(_, guards)| *guards).filter_map(|(c, _)| *c) {
        match counts.iter_mut().find(|(k, _)| *k == c) {
            Some((_, n)) => *n += 1,
            None => counts.push((c, 1)),
        }
    }
    // the lower id on a tie, so that the answer does not depend on the listing's order
    counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0))).map(|(c, _)| c)
}

/// With every session in another coalition: ask for a shell there and
/// exit when it answers.  Returns when this process is to carry on.
pub(crate) fn hand_over_if_outside(resident: bool) {
    let sessions = kept_sessions();
    let census: Vec<(Option<u64>, bool)> = sessions.iter().map(|s| (s.coalition, s.guards)).collect();
    let Some(target) = coalition_to_join(own_coalition(), &census) else { return };
    let _ = std::fs::remove_file(owner::heir_path());
    if let Err(e) = record(Some(target), resident).write() {
        return lx_warn!("shell.owner.record_failed", &format!("{e}"));
    }
    lx_event!(
        "SHELL_HEIR_WANTED",
        "every session is in another coalition; asking for a shell to be started in it",
        coalition = target,
        own = own_coalition().unwrap_or(0),
        n_sessions = sessions.len()
    );
    let deadline = Instant::now() + HEIR_WAIT;
    while Instant::now() < deadline {
        let heir = std::fs::read_to_string(owner::heir_path()).ok().and_then(|t| t.trim().parse::<i32>().ok());
        if let Some(pid) = heir {
            lx_event!("SHELL_HANDOVER", "a shell in the sessions' coalition takes over; exiting", heir = pid);
            // the lock goes with this process, and the heir is waiting for it
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    lx_warn!("shell.owner.no_heir", "no session started a shell; carrying on from outside their coalition");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_main_executable_names_its_bundle() {
        let p = std::path::Path::new;
        assert_eq!(bundle_of(p("/A B/Marspot.app/Contents/MacOS/marspot-shell")), Some(p("/A B/Marspot.app")));
        assert_eq!(bundle_of(p("/s/binaries/current/marspot-shell")), None);
        assert_eq!(bundle_of(p("/w/target/release/marspot-shell")), None);
    }

    #[test]
    fn a_shell_moves_only_when_no_session_is_with_it() {
        let with_us = [(Some(7), false), (Some(3), true), (Some(3), true)];
        assert_eq!(coalition_to_join(Some(7), &with_us), None);
        assert_eq!(coalition_to_join(Some(7), &[]), None);
        assert_eq!(coalition_to_join(None, &[(Some(3), true)]), None, "unknown is not elsewhere");
    }

    #[test]
    fn it_moves_to_where_most_sessions_can_start_it() {
        let s = [(Some(3), true), (Some(5), true), (Some(5), true), (Some(9), false), (Some(9), false), (Some(9), false)];
        assert_eq!(coalition_to_join(Some(7), &s), Some(5), "the three in 9 cannot start a shell");
        assert_eq!(coalition_to_join(Some(7), &[(Some(9), false)]), None);
        assert_eq!(coalition_to_join(Some(7), &[(Some(5), true), (Some(3), true)]), Some(3), "a tie goes to the lower id");
        assert_eq!(coalition_to_join(Some(7), &[(None, true)]), None);
    }
}

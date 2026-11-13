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
//!
//! A shell that is in the sessions' coalition and is not the process
//! LaunchServices holds answerable for it cannot become that process:
//! a coalition is given to a record when the record is made.  It
//! starts its own heir, a new process, and leaves.

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
/// executable (`owner::shell_to_start` says why it has to be that).
/// It is what this process was started as, or, for an image this
/// process moved to since (an update), in the directory it came from,
/// kept in `MARSPOT_BUNDLE_DIR`.  A shell that has neither, one that
/// was itself started from a copy outside the bundle, takes the bundle
/// on file.
pub(crate) fn successor_exe() -> PathBuf {
    let own = std::env::current_exe().unwrap_or_default();
    let came_from = std::env::var_os("MARSPOT_BUNDLE_DIR").map(|dir| PathBuf::from(dir).join("marspot-shell"));
    let exe = [Some(own.clone()), came_from]
        .into_iter()
        .flatten()
        .find(|exe| bundle_of(exe).is_some() && exe.is_file())
        .or_else(owner::installed_shell)
        .unwrap_or(own);
    owner::resolved(&exe)
}

pub(crate) use owner::bundle_of;

/// Put the bundle this shell belongs to on file, for a session and for
/// a later image of this shell that cannot tell by itself.
fn note_bundle(exe: &std::path::Path) {
    let Some(bundle) = bundle_of(exe) else { return };
    let text = format!("{}\n", bundle.display());
    if std::fs::read_to_string(owner::bundle_file()).is_ok_and(|was| was == text) {
        return;
    }
    if let Err(e) = std::fs::write(owner::bundle_file(), text) {
        lx_warn!("shell.owner.bundle_file_failed", &format!("{e}"));
    }
}

fn record(heir_in: Option<u64>, heir_resident: bool) -> Record {
    Record {
        pid: std::process::id() as i32,
        coalition: own_coalition(),
        exe: successor_exe(),
        heir_in,
        heir_resident,
        heir_env: Vec::new(),
    }
}

/// Put this process on record as the one that answers.
pub(crate) fn announce() {
    let r = record(None, false);
    note_bundle(&r.exe);
    if let Err(e) = r.write() {
        lx_warn!("shell.owner.record_failed", &format!("{e}"));
    }
}

pub(crate) struct Kept {
    sid: u64,
    coalition: Option<u64>,
    /// Whether it can start a shell when asked.
    guards: bool,
}

pub(crate) fn kept_sessions() -> Vec<Kept> {
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

/// How long a shell waits for the heir it started to say it is there.
fn leave_to(heir_wanted: &str) {
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
    lx_warn!("shell.owner.no_heir", heir_wanted);
}

/// For a shell with sessions in its coalition that LaunchServices
/// does not list the coalition under: have a shell started here, from
/// the bundle, to take this one's place, and exit when it answers.  A
/// new process is what LaunchServices gives a coalition to.
///
/// A session starts it, as for a shell outside the coalition.  One
/// this process started itself was, measured, never given the dead
/// records of the app to take over; one a session started is.  What an
/// update handed this image (the window's frame, what is still staged)
/// goes on record for it.
pub(crate) fn start_heir_here(resident: bool, sessions: &[Kept]) {
    let mine = own_coalition();
    if mine.is_none() || !sessions.iter().any(|s| s.coalition == mine && s.guards) {
        return;
    }
    let _ = std::fs::remove_file(owner::heir_path());
    let mut wanted = record(mine, resident);
    wanted.heir_env.push(crate::reclaim::count_for_heir());
    // this image took note of an update that is still to be finished
    if crate::launch::SIGUSR1_FLAG.load(std::sync::atomic::Ordering::Acquire) {
        wanted.heir_env.push((crate::resident::UPDATE_WAITING_ENV.into(), "1".into()));
    }
    if let Ok(frame) = std::env::var("MARSPOT_RESTORE_FRAME") {
        wanted.heir_env.push(("MARSPOT_RESTORE_FRAME".into(), frame));
    }
    if let Err(e) = wanted.write() {
        return lx_warn!("shell.owner.record_failed", &format!("{e}"));
    }
    lx_event!(
        "SHELL_HEIR_WANTED",
        "the coalition is not listed under this shell; asking for a shell to be started here in its place",
        coalition = mine.unwrap_or(0),
        own = mine.unwrap_or(0),
        n_sessions = sessions.len()
    );
    leave_to("no session started a shell to take this one's place; carrying on");
    announce();
}

/// With every session in another coalition: ask for a shell there and
/// exit when it answers.  Returns when this process is to carry on.
pub(crate) fn hand_over_if_outside(resident: bool) {
    let sessions = kept_sessions();
    if crate::identity::renamed() {
        start_heir_here(resident, &sessions);
    }
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
    leave_to("no session started a shell; carrying on from outside their coalition");
}

#[cfg(test)]
mod tests {
    use super::*;

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

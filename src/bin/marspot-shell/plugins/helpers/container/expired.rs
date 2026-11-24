//! A claude or codex container whose token ran out while it worked.  The
//! token in it cannot be renewed from here; once the agent on the host
//! has renewed the account's own, the container is started again with
//! that.

use std::sync::Arc;

use marspot_peer::control::AgentKind;

use super::super::notify::Stop;
use super::super::table::{Event, Helper};
use super::{Shared, docker, login, prepare, records};

/// What a stop says of the login, and whether a newer one is there.
pub(in super::super) struct Over {
    pub note: String,
    pub renewed: bool,
}

/// claude's and codex's own words for a login that is no longer taken.
pub(super) fn refused(screen: &str) -> bool {
    let s = screen.to_ascii_lowercase();
    const WORDS: [&str; 9] = [
        "token has expired",
        "please run /login",
        "api error: 401",
        "authentication_error",
        "invalid bearer token",
        "unauthorized (401)",
        "401 unauthorized",
        "failed to refresh token",
        "try signing in again",
    ];
    WORDS.iter().any(|w| s.contains(w))
}

/// Judge a stop at `now`: the token in the container ended at `expires`,
/// the account's login on the host ends at `newer`.
pub(super) fn judge(expires: i64, newer: Option<i64>, screen: &str, now: i64) -> Option<Over> {
    if expires > now {
        return None;
    }
    let ago = (now - expires) / 60_000;
    if !refused(screen) {
        return Some(Over { note: format!("The login in its container expired {ago} min ago: it may not be able to go on."), renewed: false });
    }
    let renewed = newer.is_some_and(|n| n > now);
    let then = if renewed {
        "the account's login has been renewed on the host since, so the container is started again and goes on with the same conversation"
    } else {
        "the account's login on the host has not been renewed since, so it cannot go on"
    };
    Some(Over { note: format!("It was refused: the login in its container expired {ago} min ago; {then}."), renewed })
}

/// What is to be said of `h`'s login at `stop`, `screen` being the
/// bottom of its screen.
pub(in super::super) fn at_stop(s: &Shared, h: &Helper, stop: Stop, screen: Option<&str>) -> Option<Over> {
    let agent = h.started.agent;
    if !h.contained() || !matches!(agent, AgentKind::Claude | AgentKind::Codex) || stop.is_last() || h.result_new {
        return None;
    }
    let (expires, now) = (login::expires_ms(&s.dir_of(h.sid))?, records::now_ms() as i64);
    let newer = || login::of_label(agent, &h.account).and_then(|l| l.expires_at());
    judge(expires, (expires <= now).then(newer).flatten(), screen.unwrap_or_default(), now)
}

/// Start helper `sid`'s container again with the login `account` has
/// now.  A start that fails leaves the pane at its shell, which its
/// watch tells of.
pub(in super::super) fn restart(s: &Arc<Shared>, sid: u64, agent: AgentKind, account: String) {
    let me = Arc::clone(s);
    let again = move || {
        let dir = me.dir_of(sid);
        let l = login::of_label(agent, &account).ok_or("its login cannot be read")?;
        let d = docker::find().ok_or("there is no docker command")?;
        docker::remove(&d, &docker::container_name(sid))?;
        login::hand_over(&dir, &l).map_err(|r| r.message)?;
        prepare::type_line(&me, sid, &prepare::resume_line(&dir))?;
        me.record(sid, Event::InputSeen);
        Ok::<(), String>(())
    };
    let spawned = std::thread::Builder::new().name("helpers-container".into()).spawn(move || match again() {
        Ok(()) => marspot::lx_info!("helpers.container_restarted", &format!("sid={sid}: with a newer login")),
        Err(e) => marspot::lx_warn!("helpers.container_restart_failed", &format!("sid={sid}: {e}")),
    });
    if let Err(e) = spawned {
        marspot::lx_warn!("helpers.container_restart_failed", &format!("sid={sid}: {e}"));
    }
}

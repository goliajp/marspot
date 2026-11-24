//! A claude or codex container whose token ran out while it worked.  The
//! token in it cannot be renewed from here; once the agent on the host
//! has renewed the account's own, the container is started again with
//! that.

use std::path::PathBuf;
use std::sync::Arc;

use marspot_peer::control::AgentKind;

use super::super::notify::Stop;
use super::super::table::{Event, Helper};
use super::{Shared, cleanup, docker, login, prepare, records};

/// What a stop says of the login, and whether a newer one is there.
pub(in super::super) struct Over {
    pub note: String,
    pub renewed: bool,
}

/// claude's and codex's own words for a login that is no longer taken,
/// anywhere at the bottom of the screen: trusted once the token has ended.
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

/// Non-blank rows from the bottom where codex's error line is looked for.
const LAST_ROWS: usize = 8;

/// A token refused before its end, as when the host renewed the
/// account's login and the old one stopped being taken.  Only codex's own
/// error line counts, and only near the bottom: a command's output or a
/// summary may well say "401 Unauthorized" of something else.  claude's
/// early refusal has not been seen, so it is not looked for.
pub(super) fn refused_early(agent: AgentKind, screen: &str) -> bool {
    let mut rows = screen.lines().rev().filter(|l| !l.trim().is_empty()).take(LAST_ROWS);
    agent == AgentKind::Codex && rows.any(|l| l.starts_with("■ unexpected status 401"))
}

/// Judge a stop of `agent` at `now`: the token in the container ends at
/// `expires`, the account's login on the host ends at `newer`.  A token
/// refused before its end is replaced by a newer one as one that ended.
pub(super) fn judge(agent: AgentKind, expires: i64, newer: Option<i64>, screen: &str, now: i64) -> Option<Over> {
    let refused = if expires > now { refused_early(agent, screen) } else { refused(screen) };
    if expires > now && !refused {
        return None;
    }
    let ago = (now - expires) / 60_000;
    if !refused {
        return Some(Over { note: format!("The login in its container expired {ago} min ago: it may not be able to go on."), renewed: false });
    }
    let when = if expires > now { format!("the login in its container still had {} min left", -ago) } else { format!("the login in its container expired {ago} min ago") };
    let renewed = newer.is_some_and(|n| n > now.max(expires));
    let then = if renewed {
        "the account's login has been renewed on the host since, so the container is started again and goes on with the same conversation"
    } else {
        "the account's login on the host has not been renewed since, so it cannot go on"
    };
    Some(Over { note: format!("It was refused: {when}; {then}."), renewed })
}

/// What is to be said of `h`'s login at `stop`, `screen` being the
/// bottom of its screen.
pub(in super::super) fn at_stop(s: &Shared, h: &Helper, stop: Stop, screen: Option<&str>) -> Option<Over> {
    let agent = h.started.agent;
    if !h.contained() || !matches!(agent, AgentKind::Claude | AgentKind::Codex) || stop.is_last() || h.result_new {
        return None;
    }
    let (expires, now, screen) = (login::expires_ms(&s.dir_of(h.sid))?, records::now_ms() as i64, screen.unwrap_or_default());
    let newer = || login::of_label(agent, &h.account).and_then(|l| l.expires_at());
    judge(agent, expires, (expires <= now || refused_early(agent, screen)).then(newer).flatten(), screen, now)
}

/// Start helper `sid`'s container again with the login `account` has
/// now.  A start that fails leaves the pane at its shell, which its
/// watch tells of.
pub(in super::super) fn restart(s: &Arc<Shared>, sid: u64, agent: AgentKind, account: String) {
    let me = Arc::clone(s);
    let spawned = std::thread::Builder::new().name("helpers-container".into()).spawn(move || match again(&me, sid, agent, &account, docker::find()) {
        Ok(()) => marspot::lx_info!("helpers.container_restarted", &format!("sid={sid}: with a newer login")),
        Err(e) => marspot::lx_warn!("helpers.container_restart_failed", &format!("sid={sid}: {e}")),
    });
    if let Err(e) = spawned {
        marspot::lx_warn!("helpers.container_restart_failed", &format!("sid={sid}: {e}"));
    }
}

/// The restart itself.  A helper stopped meanwhile is not started again:
/// its stop took the token away and its pane is closing.
pub(super) fn again(me: &Shared, sid: u64, agent: AgentKind, account: &str, d: Option<PathBuf>) -> Result<(), String> {
    let live = || if me.table().map.contains_key(&sid) { Ok(()) } else { Err("it was stopped meanwhile".to_string()) };
    live()?;
    let dir = me.dir_of(sid);
    let l = login::of_label(agent, account).ok_or("its login cannot be read")?;
    let d = d.ok_or("there is no docker command")?;
    docker::remove(&d, &docker::container_name(sid))?;
    live()?;
    login::hand_over(&dir, &l).map_err(|r| r.message)?;
    if let Err(e) = live() {
        cleanup::drop_tokens(&me.root(), sid);
        return Err(e);
    }
    prepare::type_line(me, sid, &prepare::resume_line(&dir))?;
    me.record(sid, Event::InputSeen);
    Ok(())
}

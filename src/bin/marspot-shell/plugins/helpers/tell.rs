//! Telling a parent of a stop of its helper, and of a result whose
//! notice a restart lost.

use std::sync::Arc;

use marspot_peer::control::AgentKind;

use super::Shared;
use super::notify::{self, Stop};
use super::pending::{Notice, SLOW_MS, front_of};
use super::table::{Event, Helper};
use super::work::{hold, send};
use crate::observe::Observation;
use crate::pane_control::names::full_name_or_entry as full_name;

/// Helper `sid`'s screen; None when it cannot be read.
pub(super) fn screen_of(sid: u64) -> Option<String> {
    marspot::pane_read::screen(sid, 0).ok()
}

/// Send the notice for a stop of `h`, and write down that it went.
pub(super) fn tell(s: &Arc<Shared>, h: &Helper, stop: Stop) {
    let sid = h.sid;
    let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
    let path = s.result_path(sid);
    let result = notify::read_result(&path);
    // claude's own stop is the plugin's report, not a guess from the screen
    let lines = match (stop, h.started.agent) {
        (Stop::Budget, _) => 2 * notify::SCREEN_LINES,
        (Stop::Idle, AgentKind::Claude) if !h.contained() => 0,
        _ => notify::SCREEN_LINES,
    };
    let full = (lines > 0).then(|| screen_of(sid)).flatten();
    let screen = full.as_deref().map(|t| notify::screen_end(t, lines));
    let over = super::container::at_stop(s, h, stop, screen.as_deref());
    let note = over.as_ref().map(|o| o.note.clone()).or_else(|| notify::refused_note(h, stop));
    let body = notify::text(&full_name(sid, &obs), h, stop, &path, result.as_deref(), screen.as_deref(), note.as_deref());
    let mut events = vec![match stop {
        Stop::Idle => Event::IdleNoticed,
        Stop::Blocked => Event::BlockedNoticed { prompt: full.as_deref().map_or_else(String::new, notify::prompt_sha) },
        Stop::Exited => Event::ExitedNoticed,
        Stop::Budget => Event::BudgetExpired,
    }];
    if let Some(r) = result.as_deref() {
        events.push(Event::ResultDelivered { sha: notify::sha(r) });
    }
    send(s, sid, Notice { parent: h.started.parent, body, events, ..Notice::default() });
    if over.is_some_and(|o| o.renewed) {
        super::container::restart(s, sid, h.started.agent, h.account.clone());
    }
}

/// Whether the stop of `h` was written while its result had not reached
/// the parent: the notice that carries the result lives in memory only,
/// and a claude or kimi helper's stop, told by its plugin once, is not
/// told again after a restart.
pub(super) fn result_untold(h: &Helper) -> bool {
    super::turn::turn_reported(h) && h.delivered_ms.is_some() && h.noticed && h.result_new
}

/// After a restart, send the parent of helper `sid`, as `obs` shows it,
/// the result of the stop that was written; only the result is written
/// when it goes.  A parent at its shell prompt cannot take it: it waits
/// for the next program there, with the slow retries for this shell
/// spent, so that each restart does not start them over.  Its give-up
/// clock (`first_failed_ms`) counts from the retell: a refusal before
/// the restart is not known.
pub(super) fn retell(s: &Arc<Shared>, sid: u64, obs: &Observation) {
    let Some(h) = s.table().map.get(&sid).cloned().filter(result_untold) else { return };
    if s.lock().undelivered.holds(sid) {
        return;
    }
    let path = s.result_path(sid);
    let Some(result) = notify::read_result(&path) else { return };
    let stop = if h.blocked_told { Stop::Blocked } else { Stop::Idle };
    let note = notify::refused_note(&h, stop);
    let body = notify::text(&full_name(sid, obs), &h, stop, &path, Some(&result), None, note.as_deref());
    let events = vec![Event::ResultDelivered { sha: notify::sha(&result) }];
    let n = Notice { parent: h.started.parent, body, events, ..Notice::default() };
    marspot::lx_info!("helpers.job", &format!("sid={sid} retell"));
    if super::tick::shell_in_front(obs, n.parent) {
        let front = front_of(obs, n.parent);
        hold(s, sid, Notice { tries: 1, front, slow: SLOW_MS.len(), first_failed_ms: Some(super::records::now_ms()), ..n });
    } else {
        send(s, sid, n);
    }
}

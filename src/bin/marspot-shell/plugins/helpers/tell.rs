//! Telling a parent of a stop of its helper.

use std::sync::Arc;

use marspot_peer::control::AgentKind;

use super::Shared;
use super::notify::{self, Stop};
use super::pending::Notice;
use super::table::{Event, Helper};
use super::work::send;
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

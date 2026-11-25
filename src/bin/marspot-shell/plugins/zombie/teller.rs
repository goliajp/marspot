//! Turning one check's suspects into messages: each pane with an item
//! never told gets one, when its session can take it and the pacing
//! lets it through.  `live` sends it to the session's inbox; the dry
//! run writes it to `dryrun.log` instead.  Either way the items the
//! message lists are then told in the pacing it was given, never again;
//! the ones it only counts wait for the next message.
//!
//! Why a due message did not go out is logged once each time the
//! reason changes: the session is busy or the person is in the pane
//! (`zombie.tell.waiting`), the pacing holds it back
//! (`zombie.would_tell.held`, with a `held` entry in `dryrun.log`
//! naming the limit), or the inbox did not take it
//! (`zombie.tell.undelivered`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use marspot::lx_info;
use marspot::zombie::check::Suspect;
use marspot::zombie::limits::Notify;
use marspot::zombie::tell::{self, Pacer, Reach};

use super::dryrun;
use crate::plugins::claudecode::PaneReach;

/// What a round of telling needs besides the suspects.
pub(super) struct Cx<'a> {
    pub notify: Notify,
    pub now: u64,
    pub scale: u64,
    /// `<state>/zombie`.
    pub dir: &'a Path,
    pub reach: &'a HashMap<u64, PaneReach>,
    /// The pane's main session has ended its turn.
    pub turn_ended: &'a dyn Fn(u64) -> bool,
}

#[derive(Default)]
pub(super) struct Teller {
    /// The reason last logged for each pane whose message is due but did not go out.
    said: HashMap<u64, String>,
}

impl Teller {
    /// Tell the panes that are due.  `send` puts a body in the inbox of
    /// the claude with a pid.  Whether any pane was told.
    pub fn tell(&mut self, cx: &Cx, suspects: &[Suspect], pacer: &mut Pacer, send: &mut dyn FnMut(i32, &str) -> Result<(), String>) -> bool {
        let mut by_pane: BTreeMap<u64, Vec<&Suspect>> = BTreeMap::new();
        for s in suspects.iter().filter(|s| !pacer.told(&s.hit.key.id)) {
            by_pane.entry(s.hit.key.sid).or_default().push(s);
        }
        let mut any = false;
        for (sid, items) in by_pane {
            let pane = cx.reach.get(&sid);
            let reach = pane.map(|p| reach_of(p, (cx.turn_ended)(sid), cx.now));
            if let Err(why) = tell::due(&items, pacer, reach.as_ref(), cx.scale) {
                if self.say(sid, why) {
                    lx_info!("zombie.tell.waiting", "a message is due when the pane can take it", sid = sid, why = why, items = items.len());
                }
                continue;
            }
            let ids: Vec<&str> = items.iter().map(|s| s.hit.key.id.as_str()).collect();
            let head = format!("sid={sid} items={}", ids.join(","));
            let log = cx.dir.join("dryrun.log");
            if let Some(limit) = pacer.holds(sid, cx.now) {
                if self.say(sid, limit) {
                    lx_info!("zombie.would_tell.held", "the pacing holds this pane's message back", sid = sid, why = limit, items = items.len());
                    dryrun::append(&log, "held", cx.now, &format!("{head} why={limit}"), "");
                }
                continue;
            }
            let (body, shown) = tell::body(&items);
            let ids = &ids[..shown];
            let head = format!("sid={sid} items={}", ids.join(","));
            match cx.notify {
                Notify::Live => {
                    let claude = pane.map_or(0, |p| p.claude_pid);
                    if let Err(e) = send(claude, &body) {
                        if self.say(sid, &e) {
                            lx_info!("zombie.tell.undelivered", &e, sid = sid, claude = claude);
                        }
                        continue;
                    }
                    lx_info!("zombie.told", "sent to the session's inbox; text in zombie/dryrun.log", sid = sid, items = ids.join(","));
                    dryrun::append(&log, "told", cx.now, &head, &body);
                }
                _ => {
                    lx_info!("zombie.would_tell", "a message this dry run did not send; text in zombie/dryrun.log", sid = sid, items = ids.join(","));
                    dryrun::append(&log, "would_tell", cx.now, &head, &body);
                }
            }
            self.said.remove(&sid);
            pacer.record(sid, cx.now, ids.iter().map(|id| id.to_string()));
            any = true;
        }
        any
    }

    /// Note `why` for `sid`; true when it differs from the last noted.
    fn say(&mut self, sid: u64, why: &str) -> bool {
        if self.said.get(&sid).is_some_and(|w| w == why) {
            return false;
        }
        self.said.insert(sid, why.to_string());
        true
    }

    pub fn keep_panes(&mut self, open: impl Fn(u64) -> bool) {
        self.said.retain(|sid, _| open(*sid));
    }
}

fn reach_of(p: &PaneReach, turn_ended: bool, now: u64) -> Reach {
    Reach {
        turn_ended,
        awaiting_for: p.awaiting_since.map(|t| now.saturating_sub(t)),
        user_here: p.user_here,
        refused: p.refused,
        dormant: p.dormant,
    }
}

#[cfg(test)]
mod tests;

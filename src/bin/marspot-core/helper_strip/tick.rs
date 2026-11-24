//! What changes a strip's height, and the size of the helpers in it.
//! Looked at once a loop turn, after everything that turn brought: the
//! loop is never woken for it but at a time something is owed.

use super::height::{Height, Said, Sight};
use super::host::Host;
use super::*;
use marspot::shell_proto::{HelperStrip, StripAgent};

/// How long after its start a core waits to be told of helpers: a shell
/// from before the message that there are no more says nothing.
const TOLD_WITHIN: Duration = Duration::from_secs(5);
/// How long after a core's start a pane can be one it found.
const FOUND_WITHIN: Duration = Duration::from_secs(30);
/// How long a pane must have kept its size before its helpers get it.
const SETTLED: Duration = Duration::from_millis(500);

fn earlier(a: &mut Option<Instant>, b: Option<Instant>) {
    *a = match (*a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    };
}

impl Strips {
    /// The user typed, pasted or composed in the pane of `sid`.
    pub(in super::super) fn typed(&mut self, sid: u64) {
        if self.on {
            self.typed.insert(sid, Instant::now());
        }
    }

    /// `sid` was given a size by whoever asked: it keeps it.
    pub(in super::super) fn sized(&mut self, sid: u64) {
        self.sized.insert(sid);
    }

    /// Whether what was not told of has no helpers.
    fn all_told(&mut self, records: &Records, now: Instant) -> bool {
        self.told |= records.all_told() || self.born.is_some_and(|t| now >= t + TOLD_WITHIN);
        self.told
    }

    /// Take in the records that came since the last look, if any did.
    fn hear(&mut self, records: &Records) -> bool {
        if std::mem::replace(&mut self.heard, records.changes()) == records.changes() {
            return false;
        }
        for r in records.all() {
            if let Some(h) = self.hosts.get_mut(&r.root_sid) {
                h.heard(r);
            } else if !r.is_empty() {
                let mut h = Host::default();
                h.heard(r);
                self.hosts.insert(r.root_sid, h);
            }
        }
        true
    }
}

impl CoreApp {
    /// Once per loop turn.
    pub(in super::super) fn strips_tick(&mut self) {
        let now = Instant::now();
        self.ripen_strips(now);
        let on = marspot::settings::get().helper_strip;
        if on != self.strips.on {
            self.turn_strips(on);
        }
        if on {
            self.strips.all_told(&self.pane_control.strips, now);
        }
        if !on || (self.strips.hosts.is_empty() && self.strips.heard == self.pane_control.strips.changes()) {
            return;
        }
        if self.strips.hear(&self.pane_control.strips) {
            for w in self.windows.iter_mut().filter(|w| w.panes.iter().any(|p| p.shelld_session_id().is_some_and(|sid| self.strips.drawn(sid)))) {
                w.needs_render = true;
            }
        }
        self.strip_heights(now);
        self.follow_hosts(now);
        self.tick_strips(now);
    }

    /// Ask for the frames in which strips move.
    fn ripen_strips(&mut self, now: Instant) {
        for (sid, strip) in self.strips.by_sid.iter_mut() {
            if strip.ripen(now)
                && let Some(w) = self.windows.iter_mut().find(|w| w.panes.iter().any(|p| p.shelld_session_id() == Some(*sid)))
            {
                w.needs_render = true;
            }
        }
    }

    /// The setting was turned: every pane with a strip gets its rows
    /// back, or gets one anew.
    fn turn_strips(&mut self, on: bool) {
        self.strips.on = on;
        self.strips.heard = 0;
        if on {
            return;
        }
        self.strips.due = None;
        self.strips.typed.clear();
        for sid in std::mem::take(&mut self.strips.hosts).into_keys() {
            self.strips.forget(sid);
            if let Some((wi, _)) = self.find_window_pane_by_sid(sid) {
                self.rebuild_layout(wi);
            }
        }
    }

    /// Whether the pane of `sid` has Claude drawing on its main screen.
    pub(super) fn strip_inline(&self, sid: u64, pane: Option<&Pane>, record: Option<&HelperStrip>) -> bool {
        let claude = match record.filter(|r| !r.is_empty()) {
            Some(r) => r.parent_agent == StripAgent::Claude,
            None => self.pane_agent_tui.get(&sid) == Some(&true),
        };
        claude && pane.is_some_and(|p| !p.session().l3_alt_screen_active())
    }

    fn strip_sight(&self, sid: u64, pane: Option<&Pane>, record: Option<&HelperStrip>, said: Said, now: Instant) -> Sight {
        Sight {
            said,
            inline: self.strip_inline(sid, pane, record),
            held: self.pane_sessions.contains_key(&sid),
            typed: self.strips.typed.get(&sid).copied(),
            now,
        }
    }

    /// Bring every strip to the height it should have.
    fn strip_heights(&mut self, now: Instant) {
        let mut strips = std::mem::take(&mut self.strips.hosts);
        let told = self.strips.all_told(&self.pane_control.strips, now);
        let waits = self.strips.waiting && !told;
        let mut due = self.strips.born.filter(|_| waits).map(|t| t + TOLD_WITHIN);
        let mut resized = Vec::new();
        strips.retain(|&sid, host| {
            let record = self.pane_control.strips.of(sid);
            let at = self.find_window_pane_by_sid(sid);
            let pane = at.map(|(wi, i)| &self.windows[wi].panes[i]);
            host.see(|helper| self.shadows.iter().any(|p| p.is_l3() && p.shelld_session_id() == Some(helper)));
            let sight = self.strip_sight(sid, pane, record, host.said(record, told), now);
            let before = host.height.rows();
            if let Some(rows) = host.height.step(&sight) {
                lx_event!("HELPER_STRIP_HEIGHT", "a pane's helper strip takes other rows", session = sid, from = before, to = rows);
                resized.push((sid, rows, at.map(|a| a.0)));
            }
            earlier(&mut due, host.height.due(&sight));
            let ticks = record.is_some_and(|r| host.ticking(r));
            host.ticks = ticks;
            host.height.rows() > 0 || sight.said != Said::Nothing
        });
        self.strips.hosts = strips;
        self.strips.due = due;
        for &(sid, rows, _) in &resized {
            self.strips.set_rows(sid, rows);
        }
        for (i, wi) in resized.iter().enumerate().filter_map(|(i, r)| Some((i, r.2?))) {
            if !resized[..i].iter().any(|r| r.2 == Some(wi)) {
                self.rebuild_layout(wi);
            }
        }
    }

    /// Give helpers the size of the terminal they are listed under,
    /// once it has settled.  One whose size was asked for keeps that.
    fn follow_hosts(&mut self, now: Instant) {
        let mut due = self.strips.due;
        let blocks: Vec<_> = self.strips.hosts.keys().map(|&sid| (sid, self.helper_block(sid, 0))).collect();
        for (sid, block) in blocks {
            let Some(host) = self.strips.hosts.get_mut(&sid) else { continue };
            let Some(size) = block else {
                host.block = None;
                continue;
            };
            let since = match host.block {
                Some((was, since)) if was == size => since,
                _ => now,
            };
            host.block = Some((size, since));
            if now < since + SETTLED {
                earlier(&mut due, Some(since + SETTLED));
                continue;
            }
            let Some(record) = self.pane_control.strips.of(sid) else { continue };
            for r in record.rows.iter().filter(|r| !self.strips.sized.contains(&r.sid)) {
                if let Some(p) = self.shadows.iter_mut().find(|p| p.is_l3() && p.shelld_session_id() == Some(r.sid)) {
                    p.resize(size.0, size.1);
                }
            }
        }
        self.strips.due = due;
    }

    /// Whether a strip drawn in window `wi` has figures that change by
    /// the second, and a frame can show them.
    fn strips_run(&self, wi: usize) -> bool {
        let w = &self.windows[wi];
        frames::paced()
            && !w.needs_render
            && !w.render.frame_in_flight()
            && !frames::waits(w)
            && w.panes.iter().filter_map(|p| p.shelld_session_id()).any(|sid| {
                self.strips.drawn(sid) && self.strips.hosts.get(&sid).is_some_and(|h| h.ticks)
            })
    }

    fn second_due(&self, wi: usize) -> Option<Instant> {
        if !self.strips_run(wi) {
            return None;
        }
        self.strips.clocks.due(self.windows[wi].window_id)
    }

    /// Ask for a frame in each window whose strips' second is over.
    fn tick_strips(&mut self, now: Instant) {
        for wi in 0..self.windows.len() {
            if self.second_due(wi).is_some_and(|t| now >= t) {
                self.windows[wi].needs_render = true;
            }
        }
    }

    /// How long until a strip has something to do.
    pub(in super::super) fn strips_wait(&self, now: Instant) -> Option<Duration> {
        if self.strips.is_empty() && self.strips.hosts.is_empty() {
            return None;
        }
        let mut due = self.strips.due;
        earlier(&mut due, self.strips.by_sid.values().filter_map(Strip::due).min());
        for wi in 0..self.windows.len() {
            earlier(&mut due, self.second_due(wi));
        }
        due.map(|t| t.saturating_duration_since(now))
    }

    /// Before window `wi` takes `layout`, which no strip has taken rows
    /// from: a session an earlier core left short of rows keeps its
    /// size, and one that is resized anyway gives up spare rows.
    pub(super) fn strips_meet(&mut self, wi: usize, layout: &Layout) {
        if !self.strips.on {
            return;
        }
        if !self.pane_control.shadows.family.counted() {
            self.shadows_changed();
        }
        for (pane, cell) in self.windows[wi].panes.iter().zip(&layout.cells) {
            let Some(sid) = pane.session().l3_session_id() else { continue };
            if self.strips.met.insert(sid) && !self.strips.hosts.contains_key(&sid) {
                let record = self.pane_control.strips.of(sid).filter(|r| !r.is_empty());
                let g = pane.session().grid();
                let short = cell.rows.saturating_sub(g.rows());
                let known = record.is_some() || self.pane_control.shadows.family.count(sid) > 0;
                let fresh = self.strips.born.is_none_or(|t| t.elapsed() < FOUND_WITHIN);
                let waiting = record.is_some() || (!self.strips.told && fresh);
                if g.cols() == cell.cols && Height::could_be(short) && waiting && (known || short == MAX_ROWS) {
                    lx_event!("HELPER_STRIP_FOUND", "a session is as short of rows as a helper strip leaves one", session = sid, rows = short);
                    let mut host = Host::found(short);
                    if let Some(r) = record {
                        host.heard(r);
                    }
                    self.strips.hosts.insert(sid, host);
                    self.strips.set_rows(sid, short);
                    self.strips.waiting = true;
                }
            }
            let Some(host) = self.strips.hosts.get_mut(&sid) else { continue };
            let cells = (cell.cols, cell.rows);
            let moved = host.cells.replace(cells).is_some_and(|was| was != cells);
            if moved && host.height.resized() {
                self.strips.set_rows(sid, 0);
            }
        }
    }
}

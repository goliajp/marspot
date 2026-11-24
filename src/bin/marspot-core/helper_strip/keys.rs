//! The keyboard in a strip, as in Claude Code's own list of subagents.
//!
//! `↓` in an empty input box goes to the agent as always; if its screen
//! has not changed 100 ms later, the agent had no use for it and the
//! strip takes the keys, until the next key at the latest that is only
//! for as long as the screen stays as it was.  There `↑ ↓` select,
//! `Enter` views, `x` stops a helper and then clears its row, and `Esc`
//! or `↑` on the first row go back to the input box; any other key goes
//! back there too, and to the agent.

use super::compose::{Pick, State};
use super::native_list;
use super::probe::{look, Look};
use super::*;
use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::shell_proto::{encode_strip_keys, StripAgent, StripKeys};

/// How long the agent has to show it took a key.
pub(super) const PROBE: Duration = Duration::from_millis(100);

/// Which pane, by its window and its session, and which session it
/// shows: what has the keys is only good while the same is in front.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct At {
    window_id: u32,
    host: u64,
    shown: u64,
}

struct Probe {
    at: At,
    look: Look,
    due: Instant,
}

pub(super) struct Focus {
    at: At,
    pub selected: Pick,
    /// How the screen was when the keys were taken; none once a key
    /// came since.
    provisional: Option<Look>,
}

#[derive(Default)]
pub(in super::super) struct Keys {
    probe: Option<Probe>,
    pub(super) focus: Option<Focus>,
    /// The shell lets a plain `x` through to the core as a key.
    shell_takes_x: bool,
}

impl Keys {
    pub(super) fn due(&self) -> Option<Instant> {
        self.probe.as_ref().map(|p| p.due)
    }

    /// What is selected in the strip of `host` in window `window_id`.
    pub(super) fn selected(&self, window_id: u32, host: u64) -> Option<Pick> {
        self.focus.as_ref().filter(|f| (f.at.window_id, f.at.host) == (window_id, host)).map(|f| f.selected)
    }
}

fn plain(m: Modifiers) -> bool {
    !(m.shift || m.control || m.alt || m.super_)
}

impl CoreApp {
    /// The shell said it lets `x` through.
    pub(in super::super) fn shell_takes_strip_x(&mut self, payload: &[u8]) {
        if marspot::shell_proto::decode_strip_keys(payload).is_ok() {
            self.strips.keys.shell_takes_x = true;
        }
    }

    /// The focused slot of window `wi`, if a strip is drawn under it
    /// and what it shows is live.
    fn strip_at_focus(&self, wi: usize) -> Option<At> {
        let w = self.windows.get(wi)?;
        let host = w.panes.get(w.focused_idx)?.shelld_session_id()?;
        let shown = self.focused_shown(wi).filter(|p| p.is_l3() && p.view_offset() == 0)?;
        self.strips.drawn(host).then_some(At { window_id: w.window_id, host, shown: shown.shelld_session_id()? })
    }

    /// How the screen in front of `at` looks, if it has an empty input
    /// box with the cursor in it.
    fn look_at(&self, at: At) -> Option<Look> {
        let (wi, _) = self.slot_showing(at.shown)?;
        if self.windows[wi].window_id != at.window_id || !self.windows[wi].ime_preedit.is_empty() {
            return None;
        }
        let pane = self.focused_shown(wi).filter(|p| p.shelld_session_id() == Some(at.shown))?;
        look(pane.session().grid(), pane.session().cursor_visible())
    }

    /// The agent in front of `at`, as the shell says.
    fn agent_at(&self, at: At) -> StripAgent {
        let Some(record) = self.pane_control.strips.of(at.host) else { return StripAgent::Other };
        match record.rows.iter().find(|r| r.sid == at.shown) {
            Some(r) => r.agent,
            None => record.parent_agent,
        }
    }

    /// A key for the focused slot of window `wi`: whether the strip took
    /// it.  One it does not take goes on to the program.
    pub(in super::super) fn strip_key(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) -> bool {
        if event.state != KeyState::Pressed || self.strips.is_empty() {
            return false;
        }
        let window_id = self.windows[wi].window_id;
        if self.strips.keys.focus.as_ref().is_some_and(|f| f.at.window_id == window_id) {
            return self.focused_strip_key(wi, event, modifiers);
        }
        if event.repeat {
            return false;
        }
        self.strips.keys.probe = None;
        if event.logical == LogicalKey::Named(NamedKey::ArrowDown) && plain(modifiers) {
            self.start_strip_probe(wi);
        }
        false
    }

    fn start_strip_probe(&mut self, wi: usize) {
        let Some(at) = self.strip_at_focus(wi) else { return };
        if self.agent_at(at) == StripAgent::Gemini {
            return;
        }
        let Some(look) = self.look_at(at) else { return };
        // on a list of its own the agent uses the key, unless what is
        // selected is the last row of it
        let screen = self.shown(wi, self.windows[wi].focused_idx).session().grid();
        if native_list::find(screen).is_some_and(|n| n.selected.is_some() && !n.on_last) {
            return;
        }
        self.strips.keys.probe = Some(Probe { at, look, due: Instant::now() + PROBE });
    }

    /// After a pump: a probe that is due takes the keys or gives up,
    /// and keys taken go back when the pane is no longer as it was.
    pub(super) fn strip_keys_tick(&mut self, now: Instant) {
        if let Some(p) = self.strips.keys.probe.take_if(|p| now >= p.due) {
            let still = self.focused_at(p.at) && self.look_at(p.at) == Some(p.look);
            if still {
                lx_event!("HELPER_STRIP_KEYS", "the strip takes the keys", host = p.at.host);
                self.take_strip_keys(Focus { at: p.at, selected: Pick::Row(0), provisional: Some(p.look) });
            }
        }
        let Some(f) = &self.strips.keys.focus else { return };
        let at = f.at;
        let moved = f.provisional.is_some_and(|was| self.look_at(at) != Some(was));
        if moved || !self.focused_at(at) {
            self.leave_strip_keys();
        }
    }

    /// Whether `at` is what the focused slot of its window shows, with
    /// a strip under it.
    fn focused_at(&self, at: At) -> bool {
        let wi = self.windows.iter().position(|w| w.window_id == at.window_id);
        wi.and_then(|wi| self.strip_at_focus(wi)) == Some(at)
    }

    fn take_strip_keys(&mut self, f: Focus) {
        self.tell_shell_strip_keys(f.at.window_id, true);
        self.strip_window_changed(f.at.window_id);
        self.strips.keys.focus = Some(f);
    }

    pub(in super::super) fn leave_strip_keys(&mut self) {
        self.strips.keys.probe = None;
        if let Some(f) = self.strips.keys.focus.take() {
            self.tell_shell_strip_keys(f.at.window_id, false);
            self.strip_window_changed(f.at.window_id);
        }
    }

    fn tell_shell_strip_keys(&mut self, window_id: u32, on: bool) {
        self.pending_to_shell.push((MsgType::StripKeys, encode_strip_keys(&StripKeys { window_id, on })));
    }

    fn strip_window_changed(&mut self, window_id: u32) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.window_id == window_id) {
            w.needs_render = true;
        }
    }

    /// A key while the strip has the keys.
    fn focused_strip_key(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) -> bool {
        let Some(f) = self.strips.keys.focus.as_mut() else { return false };
        f.provisional = None;
        if event.repeat {
            // held since before, or held here: either way not the agent's
            return true;
        }
        let (at, selected) = (f.at, f.selected);
        let Some((rows, main)) = self.strip_rows_at(at) else {
            self.leave_strip_keys();
            return false;
        };
        let last = rows.saturating_sub(1);
        let to = match (&event.logical, plain(modifiers)) {
            (LogicalKey::Named(NamedKey::ArrowUp), true) => match selected {
                Pick::Row(0) if main => Some(Pick::Main),
                Pick::Row(i) if i > 0 => Some(Pick::Row(i.min(rows) - 1)),
                _ => None,
            },
            (LogicalKey::Named(NamedKey::ArrowDown), true) => match selected {
                Pick::Main => Some(Pick::Row(0)),
                Pick::Row(i) => Some(Pick::Row((i + 1).min(last))),
            },
            (LogicalKey::Named(NamedKey::Enter), true) => return self.strip_enter(at, selected, &event.logical),
            (LogicalKey::Named(NamedKey::Escape), _) => None,
            (LogicalKey::Char('x'), true) if self.strips.keys.shell_takes_x => return self.strip_x(wi, at, selected, event),
            _ => {
                self.leave_strip_keys();
                return false;
            }
        };
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        match to {
            Some(pick) => {
                if let Some(f) = self.strips.keys.focus.as_mut() {
                    f.selected = pick;
                }
                self.strip_window_changed(at.window_id);
            }
            None => self.leave_strip_keys(),
        }
        true
    }

    /// How many helpers the strip of `at` lists, and whether it draws a
    /// `main` of its own.
    fn strip_rows_at(&self, at: At) -> Option<(usize, bool)> {
        let host = self.strips.hosts.get(&at.host)?;
        let record = self.pane_control.strips.of(at.host)?;
        let rows = host.shown_rows(record);
        (rows > 0).then_some((rows, host.drawn.has_main()))
    }

    fn strip_enter(&mut self, at: At, selected: Pick, key: &LogicalKey) -> bool {
        self.pane_control.shadows.peeks.held.swallowed(key);
        self.leave_strip_keys();
        let helper = match selected {
            Pick::Main => None,
            Pick::Row(i) => self.pane_control.strips.of(at.host).and_then(|r| self.strips.hosts.get(&at.host)?.sid_at(r, i)),
        };
        if helper != Some(at.shown) && (helper.is_some() || at.shown != at.host) {
            self.view_helper(at.host, helper);
        }
        true
    }

    /// `x` on a helper at work stops it; on one that is not, clears its
    /// row.  On the row of what is viewed it is a key like any other.
    fn strip_x(&mut self, wi: usize, at: At, selected: Pick, event: &MarspotKeyEvent) -> bool {
        let Pick::Row(i) = selected else {
            self.leave_strip_keys();
            return false;
        };
        let record = self.pane_control.strips.of(at.host);
        let host = self.strips.hosts.get_mut(&at.host);
        let found = record.zip(host).and_then(|(r, h)| h.sid_at(r, i).zip(h.state_at(r, i)));
        let Some((sid, state)) = found.filter(|f| f.0 != at.shown) else {
            self.leave_strip_keys();
            return false;
        };
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        if matches!(state, State::Running | State::Awaiting) {
            lx_event!("HELPER_STRIP_STOP", "x in the strip stops a helper", session = sid);
            self.stop_helper(sid);
        } else if let Some(host) = self.strips.hosts.get_mut(&at.host) {
            host.hide(sid);
            if let Some(f) = self.strips.keys.focus.as_mut() {
                f.selected = Pick::Row(i.saturating_sub(1));
            }
        }
        self.windows[wi].needs_render = true;
        true
    }

    /// Something other than a key happened in the strip's pane: what
    /// was waited for or taken goes.
    pub(in super::super) fn strip_keys_interrupted(&mut self) {
        if self.strips.keys.probe.is_some() || self.strips.keys.focus.is_some() {
            self.leave_strip_keys();
        }
    }
}

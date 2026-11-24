//! The keyboard in a strip, as in Claude Code's own list of subagents.
//!
//! `↓` in an empty input box goes to the agent as always; if its screen
//! has not changed 100 ms later, the agent had no use for it and the
//! strip takes the keys, until the next key at the latest that is only
//! for as long as the screen stays as it was.  There `↑ ↓` select,
//! `Enter` views, `x` stops a helper and then clears its row, and `Esc`
//! or `↑` on the first row go back to the input box; any other key goes
//! back there too, and to the agent.

use super::compose::Pick;
use super::native_list;
use super::probe::{look, Look};
use super::*;
use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::shell_proto::StripAgent;

/// How long the agent has to show it took a key.
pub(super) const PROBE: Duration = Duration::from_millis(100);

/// Which pane, by its window and its session, and which session it
/// shows: what has the keys is only good while the same is in front.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct At {
    pub(super) window_id: u32,
    pub(super) host: u64,
    pub(super) shown: u64,
}

struct Probe {
    at: At,
    look: Look,
    due: Instant,
}

pub(super) struct Focus {
    pub(super) at: At,
    pub selected: Pick,
    /// How the screen was when the keys were taken, and the frame last
    /// looked at; none once a key came since.
    provisional: Option<(Look, u64)>,
}

#[derive(Default)]
pub(in super::super) struct Keys {
    probe: Option<Probe>,
    pub(super) focus: Option<Focus>,
    /// The shell lets a plain `x` through to the core as a key.
    pub(super) shell_takes_x: bool,
    /// The window the shell was last told lets its `x` through.
    pub(super) x_told: Option<u32>,
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

/// `selected` within `rows` rows, and a strip with no `main` of its own.
fn clamped(selected: Pick, rows: usize, main: bool) -> Pick {
    match selected {
        Pick::Main if main => Pick::Main,
        Pick::Main => Pick::Row(0),
        Pick::Row(i) => Pick::Row(i.min(rows.saturating_sub(1))),
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
        let pane = self.shown_at(at)?;
        look(pane.session().grid(), pane.session().cursor_visible())
    }

    /// What is in front of `at`, with nothing being composed over it.
    fn shown_at(&self, at: At) -> Option<&Pane> {
        let (wi, _) = self.slot_showing(at.shown)?;
        if self.windows[wi].window_id != at.window_id || !self.windows[wi].ime_preedit.is_empty() {
            return None;
        }
        self.focused_shown(wi).filter(|p| p.shelld_session_id() == Some(at.shown))
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
                let seq = self.shown_at(p.at).map_or(0, Pane::grid_seq);
                self.take_strip_keys(Focus { at: p.at, selected: Pick::Row(0), provisional: Some((p.look, seq)) });
            }
        }
        let Some(f) = &self.strips.keys.focus else { return };
        let (at, provisional) = (f.at, f.provisional);
        // a frame is looked at once
        let seq = self.shown_at(at).map(Pane::grid_seq);
        let moved = provisional.is_some_and(|(was, seen)| Some(seen) != seq && self.look_at(at) != Some(was));
        let rows = self.strip_rows_at(at);
        if moved || rows.is_none() || !self.focused_at(at) {
            self.leave_strip_keys();
            return;
        }
        if let (Some(f), Some((rows, main))) = (self.strips.keys.focus.as_mut(), rows) {
            f.selected = clamped(f.selected, rows, main);
            if let Some(seq) = seq {
                f.provisional = f.provisional.map(|(was, _)| (was, seq));
            }
        }
        self.tell_shell_strip_x();
    }

    /// Whether `at` is what the focused slot of its window shows, with
    /// a strip under it.
    fn focused_at(&self, at: At) -> bool {
        let wi = self.windows.iter().position(|w| w.window_id == at.window_id);
        wi.and_then(|wi| self.strip_at_focus(wi)) == Some(at)
    }

    fn take_strip_keys(&mut self, f: Focus) {
        // keys held in another window go back there first
        self.leave_strip_keys();
        self.strip_window_changed(f.at.window_id);
        self.strips.keys.focus = Some(f);
        self.tell_shell_strip_x();
    }

    pub(in super::super) fn leave_strip_keys(&mut self) {
        self.strips.keys.probe = None;
        if let Some(f) = self.strips.keys.focus.take() {
            self.strip_window_changed(f.at.window_id);
        }
        self.tell_shell_strip_x();
    }

    pub(super) fn strip_window_changed(&mut self, window_id: u32) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.window_id == window_id) {
            w.needs_render = true;
        }
    }

    /// A key while the strip has the keys.
    fn focused_strip_key(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) -> bool {
        let Some(at) = self.strips.keys.focus.as_ref().map(|f| f.at) else { return false };
        // the focus moved since: the key is for what is focused now
        if !self.focused_at(at) {
            self.leave_strip_keys();
            return false;
        }
        let Some(f) = self.strips.keys.focus.as_mut() else { return false };
        f.provisional = None;
        let was = f.selected;
        if event.repeat {
            // held since before, or held here: either way not the agent's
            return true;
        }
        let Some((rows, main)) = self.strip_rows_at(at) else {
            self.leave_strip_keys();
            return false;
        };
        let selected = clamped(was, rows, main);
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
                self.tell_shell_strip_x();
            }
            None => self.leave_strip_keys(),
        }
        true
    }

    /// How many helpers the strip of `at` lists, and whether it draws a
    /// `main` of its own.
    pub(super) fn strip_rows_at(&self, at: At) -> Option<(usize, bool)> {
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

    /// Something other than a key happened in the strip's pane: what
    /// was waited for or taken goes.
    pub(in super::super) fn strip_keys_interrupted(&mut self) {
        if self.strips.keys.probe.is_some() || self.strips.keys.focus.is_some() {
            self.leave_strip_keys();
        }
    }
}

//! A plain `x` in a strip.  The shell lets it past the input method
//! only while the row selected is one `x` acts on, so that anywhere
//! else it is typed as it would be.

use super::compose::{Pick, State};
use super::keys::{At, Focus};
use super::*;
use marspot::shell_proto::{encode_strip_keys, StripKeys};

impl CoreApp {
    /// `x` on a helper at work stops it; on one that is not, clears its
    /// row.  On the row of what is viewed it is a key like any other.
    pub(super) fn strip_x(&mut self, wi: usize, at: At, selected: Pick, event: &MarspotKeyEvent) -> bool {
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
        if matches!(state, State::Running | State::Awaiting | State::Idle) {
            lx_event!("HELPER_STRIP_STOP", "x in the strip stops a helper", session = sid);
            self.stop_helper(sid);
        } else if let Some(host) = self.strips.hosts.get_mut(&at.host) {
            host.hide(sid);
            if let Some(f) = self.strips.keys.focus.as_mut() {
                f.selected = Pick::Row(i.saturating_sub(1));
            }
            self.tell_shell_strip_x();
        }
        self.windows[wi].needs_render = true;
        true
    }

    /// Whether `x` does something on the row selected in `f`.
    fn x_acts(&self, f: &Focus) -> bool {
        let Pick::Row(i) = f.selected else { return false };
        let record = self.pane_control.strips.of(f.at.host);
        let host = self.strips.hosts.get(&f.at.host);
        record.zip(host).and_then(|(r, h)| h.sid_at(r, i)).is_some_and(|sid| sid != f.at.shown)
    }

    /// Tells the shell, when it changes, which window lets its `x`
    /// through, if any.
    pub(super) fn tell_shell_strip_x(&mut self) {
        let want = self.strips.keys.focus.as_ref().filter(|f| self.x_acts(f)).map(|f| f.at.window_id);
        let told = self.strips.keys.x_told;
        if want == told {
            return;
        }
        if let Some(window_id) = told {
            self.pending_to_shell.push((MsgType::StripKeys, encode_strip_keys(&StripKeys { window_id, on: false })));
        }
        if let Some(window_id) = want {
            self.pending_to_shell.push((MsgType::StripKeys, encode_strip_keys(&StripKeys { window_id, on: true })));
        }
        self.strips.keys.x_told = want;
    }
}

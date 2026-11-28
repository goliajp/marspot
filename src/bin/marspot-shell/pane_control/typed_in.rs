//! When each pane last had input: a key while it had the keyboard, a
//! send through the control socket, or a file drop or mouse report the
//! core told of.
//!
//! The key path pays only its one clock store (`Hub::key_pressed`).
//! Which pane a key went to is settled when the keyboard moves (the
//! core's `KeyboardPane`): a key pressed since the keyboard came to a
//! pane went to that pane.

use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

/// Panes remembered; the one with the oldest input goes.
const CAP: usize = 256;

#[derive(Default)]
pub(super) struct TypedIn(Mutex<Inner>);

#[derive(Default)]
struct Inner {
    focus: u64,
    focus_since: Option<SystemTime>,
    last: Vec<(u64, SystemTime)>,
}

impl Inner {
    fn note(&mut self, sid: u64, at: SystemTime) {
        match self.last.iter_mut().find(|(s, _)| *s == sid) {
            Some(e) => e.1 = e.1.max(at),
            None => {
                if self.last.len() >= CAP
                    && let Some(i) = self.last.iter().enumerate().min_by_key(|(_, e)| e.1).map(|(i, _)| i)
                {
                    self.last.swap_remove(i);
                }
                self.last.push((sid, at));
            }
        }
    }

    /// The last key, when it went to the pane that has the focus.
    fn key_in_focus(&self, key: Option<SystemTime>) -> Option<SystemTime> {
        key.filter(|k| self.focus != 0 && self.focus_since.is_some_and(|f| *k >= f))
    }
}

impl TypedIn {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The focus moves to `sid` at `now`; `key` is when the last key
    /// was pressed.
    pub(super) fn focused(&self, sid: u64, key: Option<SystemTime>, now: SystemTime) {
        let mut g = self.inner();
        if let Some(k) = g.key_in_focus(key) {
            let was = g.focus;
            g.note(was, k);
        }
        g.focus = sid;
        g.focus_since = Some(now);
    }

    /// Something was written into pane `sid` at `at`.
    pub(super) fn sent(&self, sid: u64, at: SystemTime) {
        self.inner().note(sid, at);
    }

    /// When pane `sid` last had input, as far as this process saw.
    pub(super) fn last_input(&self, sid: u64, key: Option<SystemTime>) -> Option<SystemTime> {
        let g = self.inner();
        let kept = g.last.iter().find(|(s, _)| *s == sid).map(|e| e.1);
        let now = if g.focus == sid { g.key_in_focus(key) } else { None };
        kept.max(now)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(s: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(s)
    }

    #[test]
    fn a_key_belongs_to_the_pane_that_had_the_focus() {
        let t = TypedIn::default();
        t.focused(4, None, at(10));
        assert_eq!(t.last_input(4, Some(at(5))), None, "a key before the focus came");
        assert_eq!(t.last_input(4, Some(at(12))), Some(at(12)));
        assert_eq!(t.last_input(5, Some(at(12))), None);
        // the focus moves on: the key stays with pane 4
        t.focused(5, Some(at(12)), at(20));
        assert_eq!(t.last_input(4, Some(at(12))), Some(at(12)));
        assert_eq!(t.last_input(5, Some(at(12))), None);
        t.sent(6, at(30));
        assert_eq!(t.last_input(6, None), Some(at(30)));
    }

    #[test]
    fn the_record_is_capped() {
        let t = TypedIn::default();
        for sid in 0..(CAP as u64 + 10) {
            t.sent(sid + 1, at(sid));
        }
        assert_eq!(t.inner().last.len(), CAP);
        assert_eq!(t.last_input(1, None), None, "the oldest went");
        assert!(t.last_input(CAP as u64 + 10, None).is_some());
    }
}

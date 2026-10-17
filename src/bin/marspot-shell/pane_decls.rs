//! What the shell keeps the active core told about each pane: its
//! badge, its title, how far it is dimmed.
//!
//! Plugins restate these every tick, and the shell used to forward
//! every restatement to the core.  Now a value goes out when it differs
//! from what the current core was sent, and everything goes out again,
//! once, to a core that has just come up: a new core starts knowing
//! nothing, and that is the case the restatements were for.

use std::collections::{HashMap, HashSet};

use marspot::shell_proto::{self, MsgType};

/// How each pane should look, as far as the shell tells the core:
/// recede level, plugin badge, plugin title.
pub struct PaneLook {
    pub recede: PaneDecls<u32>,
    pub badges: PaneDecls<String>,
    pub titles: PaneDecls<String>,
}

impl PaneLook {
    pub fn new() -> Self {
        Self {
            recede: PaneDecls::new(),
            badges: PaneDecls::new(),
            titles: PaneDecls::new(),
        }
    }

    pub fn core_replaced(&mut self) {
        self.recede.core_replaced();
        self.badges.core_replaced();
        self.titles.core_replaced();
    }

    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.recede.retain(&keep);
        self.badges.retain(&keep);
        self.titles.retain(&keep);
    }

    /// Frame everything the current core has not been sent.  `send`
    /// returns whether the frame reached a core.
    pub fn flush(&mut self, mut send: impl FnMut(MsgType, Vec<u8>) -> bool) {
        self.badges.flush(|sid, text| {
            send(MsgType::PaneBadge, shell_proto::encode_pane_badge(sid, text))
        });
        self.titles.flush(|sid, text| {
            send(MsgType::PaneTitle, shell_proto::encode_pane_title(sid, text))
        });
        self.recede.flush(|sid, level| {
            send(MsgType::PaneRecede, shell_proto::encode_pane_recede(sid, *level))
        });
    }
}

/// One kind of per-pane value, keyed by session id.
///
/// `declared` is what the pane should show; `delivered` is what the
/// current core was actually sent.  Only a send that reached a core
/// counts as delivered, so a value declared while no core is attached
/// goes out as soon as one is.
pub struct PaneDecls<V> {
    declared: HashMap<u64, V>,
    delivered: HashMap<u64, V>,
    /// Sessions whose declared value may differ from the delivered one.
    pending: HashSet<u64>,
}

impl<V: Clone + PartialEq> PaneDecls<V> {
    pub fn new() -> Self {
        Self {
            declared: HashMap::new(),
            delivered: HashMap::new(),
            pending: HashSet::new(),
        }
    }

    /// Record what `sid` should show.  True when that differs from what
    /// was declared before.
    pub fn declare(&mut self, sid: u64, value: V) -> bool {
        if self.declared.get(&sid) == Some(&value) {
            return false;
        }
        self.declared.insert(sid, value);
        self.pending.insert(sid);
        true
    }

    /// Send every declared value the current core does not have yet.
    /// `send` returns whether the frame reached a core; one that did
    /// not stays pending.
    pub fn flush(&mut self, mut send: impl FnMut(u64, &V) -> bool) {
        self.pending.retain(|sid| {
            let Some(value) = self.declared.get(sid) else {
                return false;
            };
            if self.delivered.get(sid) == Some(value) {
                return false;
            }
            if send(*sid, value) {
                self.delivered.insert(*sid, value.clone());
                return false;
            }
            true
        });
    }

    /// A new core is attached: it has been told nothing, so everything
    /// declared is pending again.
    pub fn core_replaced(&mut self) {
        self.delivered.clear();
        self.pending.extend(self.declared.keys().copied());
    }

    /// Forget every session `keep` rejects.
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.declared.retain(|sid, _| keep(*sid));
        self.delivered.retain(|sid, _| keep(*sid));
        self.pending.retain(|sid| keep(*sid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flush_all(d: &mut PaneDecls<String>) -> Vec<(u64, String)> {
        let mut sent = Vec::new();
        d.flush(|sid, v| {
            sent.push((sid, v.clone()));
            true
        });
        sent.sort();
        sent
    }

    #[test]
    fn same_value_twice_is_sent_once() {
        let mut d = PaneDecls::new();
        assert!(d.declare(1, "a".to_string()));
        assert_eq!(flush_all(&mut d), vec![(1, "a".to_string())]);
        assert!(!d.declare(1, "a".to_string()));
        assert!(flush_all(&mut d).is_empty());
    }

    #[test]
    fn changed_value_is_sent() {
        let mut d = PaneDecls::new();
        d.declare(1, "a".to_string());
        flush_all(&mut d);
        assert!(d.declare(1, "b".to_string()));
        assert_eq!(flush_all(&mut d), vec![(1, "b".to_string())]);
    }

    #[test]
    fn change_and_change_back_before_a_flush_sends_nothing() {
        let mut d = PaneDecls::new();
        d.declare(1, "a".to_string());
        flush_all(&mut d);
        d.declare(1, "b".to_string());
        d.declare(1, "a".to_string());
        assert!(flush_all(&mut d).is_empty());
    }

    #[test]
    fn new_core_gets_everything_declared_exactly_once() {
        let mut d = PaneDecls::new();
        d.declare(1, "a".to_string());
        d.declare(2, "b".to_string());
        flush_all(&mut d);
        d.core_replaced();
        assert_eq!(
            flush_all(&mut d),
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );
        assert!(flush_all(&mut d).is_empty());
    }

    #[test]
    fn a_send_that_failed_is_sent_after_attach() {
        let mut d = PaneDecls::new();
        d.declare(1, "a".to_string());
        d.flush(|_, _| false);
        d.flush(|_, _| false);
        d.core_replaced();
        assert_eq!(flush_all(&mut d), vec![(1, "a".to_string())]);
    }

    #[test]
    fn a_send_that_failed_is_retried_without_attach() {
        let mut d = PaneDecls::new();
        d.declare(1, "a".to_string());
        d.flush(|_, _| false);
        assert_eq!(flush_all(&mut d), vec![(1, "a".to_string())]);
    }

    #[test]
    fn recede_level_survives_a_restart() {
        let mut d: PaneDecls<u32> = PaneDecls::new();
        d.declare(7, 2);
        d.flush(|_, _| true);
        d.core_replaced();
        let mut sent = Vec::new();
        d.flush(|sid, v| {
            sent.push((sid, *v));
            true
        });
        assert_eq!(sent, vec![(7, 2)]);
    }

    #[test]
    fn retained_out_sessions_are_not_resent() {
        let mut d = PaneDecls::new();
        d.declare(1, "a".to_string());
        d.declare(2, "b".to_string());
        flush_all(&mut d);
        d.retain(|sid| sid == 2);
        d.core_replaced();
        assert_eq!(flush_all(&mut d), vec![(2, "b".to_string())]);
        // a session that comes back is news again
        assert!(d.declare(1, "a".to_string()));
        assert_eq!(flush_all(&mut d), vec![(1, "a".to_string())]);
    }
}

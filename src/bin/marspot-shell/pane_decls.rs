//! What the shell keeps the active core told about each pane: its
//! badge, its title, how far it is dimmed, the helpers under it.
//!
//! Plugins restate these every tick, and the shell used to forward
//! every restatement to the core.  Now a value goes out when it differs
//! from what the current core was sent, and everything goes out again,
//! once, to a core that has just come up: a new core starts knowing
//! nothing, and that is the case the restatements were for.

use std::collections::{HashMap, HashSet};

use marspot::shell_proto::{self, CAP_HELPER_STRIP, HelperStrip, MsgType};

/// How each pane should look, as far as the shell tells the core:
/// recede level, plugin badge, plugin title, helper strip.
pub struct PaneLook {
    pub recede: PaneDecls<u32>,
    pub badges: PaneDecls<String>,
    pub titles: PaneDecls<String>,
    pub strips: PaneDecls<HelperStrip>,
    /// Everything there is to say of helpers was declared at least once.
    pub strips_known: bool,
    /// The current core was told that it has all of it.
    all_told: bool,
}

impl PaneLook {
    pub fn new() -> Self {
        Self {
            recede: PaneDecls::new(),
            badges: PaneDecls::new(),
            titles: PaneDecls::new(),
            strips: PaneDecls::new(),
            strips_known: false,
            all_told: false,
        }
    }

    pub fn core_replaced(&mut self) {
        self.recede.core_replaced();
        self.badges.core_replaced();
        self.titles.core_replaced();
        self.strips.core_replaced();
        self.all_told = false;
    }

    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.recede.retain(&keep);
        self.badges.retain(&keep);
        self.titles.retain(&keep);
        self.strips.retain(&keep);
    }

    /// Frame everything the current core, which does what `caps` says,
    /// has not been sent.  `send` returns whether the frame reached a
    /// core.
    pub fn flush(&mut self, caps: u32, mut send: impl FnMut(MsgType, Vec<u8>) -> bool) {
        self.badges.flush(|sid, text| {
            send(MsgType::PaneBadge, shell_proto::encode_pane_badge(sid, text))
        });
        self.titles.flush(|sid, text| {
            send(MsgType::PaneTitle, shell_proto::encode_pane_title(sid, text))
        });
        self.recede.flush(|sid, level| {
            send(MsgType::PaneRecede, shell_proto::encode_pane_recede(sid, *level))
        });
        if caps & CAP_HELPER_STRIP != 0 {
            self.strips.flush(|_, strip| {
                send_strip(&mut send, strip)
            });
            if self.strips_known && !self.all_told && self.strips.pending.is_empty() {
                self.all_told = send_strip(&mut send, &HelperStrip::all_told());
            }
        }
    }
}

fn send_strip(send: &mut impl FnMut(MsgType, Vec<u8>) -> bool, strip: &HelperStrip) -> bool {
    shell_proto::encode_helper_strip(strip).is_ok_and(|p| send(MsgType::HelperStrip, p))
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

    /// [`Self::declare`], cloning only what differs.
    pub fn declare_ref(&mut self, sid: u64, value: &V) {
        if self.declared.get(&sid) != Some(value) {
            self.declare(sid, value.clone());
        }
    }

    /// What was last declared for `sid`.
    pub fn declared(&self, sid: u64) -> Option<&V> {
        self.declared.get(&sid)
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

    #[test]
    fn helper_strip_goes_only_to_a_core_that_takes_it() {
        let mut look = PaneLook::new();
        look.strips.declare(7, HelperStrip::empty(7));
        look.titles.declare(7, "t".into());
        let flush = |look: &mut PaneLook, caps: u32| {
            let mut out = Vec::new();
            look.flush(caps, |t, p| {
                out.push((t, p));
                true
            });
            out
        };
        let sent = flush(&mut look, !CAP_HELPER_STRIP);
        assert_eq!(sent.iter().map(|s| s.0).collect::<Vec<_>>(), [MsgType::PaneTitle]);
        // it says so after its hello
        let sent = flush(&mut look, CAP_HELPER_STRIP);
        assert_eq!(sent.len(), 1);
        assert_eq!(shell_proto::decode_helper_strip(&sent[0].1).unwrap(), HelperStrip::empty(7));
        assert!(flush(&mut look, CAP_HELPER_STRIP).is_empty());
        look.core_replaced();
        assert_eq!(flush(&mut look, 0).len(), 1, "the new core has not said");
        assert_eq!(flush(&mut look, CAP_HELPER_STRIP).len(), 1);
    }

    #[test]
    fn helper_strip_end_marker_follows_every_record_once_a_core() {
        let mut look = PaneLook::new();
        for sid in [7, 8] {
            look.strips.declare(sid, HelperStrip::empty(sid));
        }
        let flush = |look: &mut PaneLook, fails: u64| {
            let mut out = Vec::new();
            look.flush(CAP_HELPER_STRIP, |_, p| {
                let root = shell_proto::decode_helper_strip(&p).unwrap().root_sid;
                let ok = root != fails;
                out.extend(ok.then_some(root));
                ok
            });
            out
        };
        assert_eq!(flush(&mut look, 8), [7], "not all of it was declared");
        look.strips_known = true;
        assert_eq!(flush(&mut look, 8), [], "one did not reach the core");
        assert_eq!(flush(&mut look, 0), [8], "nor did the marker");
        assert_eq!(flush(&mut look, 99), [0]);
        look.strips.declare(9, HelperStrip::empty(9));
        assert_eq!(flush(&mut look, 99), [9], "a core is told once");
        look.core_replaced();
        let again = flush(&mut look, 99);
        assert_eq!((again.len(), again.last()), (4, Some(&0)));
    }
}

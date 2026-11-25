//! How large each helper's context is, as the plugins that read its
//! agent's records find it.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use marspot_peer::control::AgentKind;

use super::super::work::Job;
use super::super::{SHARED, Shared};
use crate::plugins::peerbus::ThreadView;

/// What an agent's record says of its context.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(in crate::plugins) struct Used {
    /// Tokens of the last response: what it was sent and what it said.
    pub tokens: Option<u64>,
    /// Whether a tool was ever called; None: not known.
    pub had_tool: Option<bool>,
    /// The model that gave the last response.
    pub model: Option<Model>,
}

impl Used {
    /// What is known once `newer` was read too.
    fn then(self, newer: Used) -> Used {
        let had_tool = if self.had_tool == Some(true) { self.had_tool } else { newer.had_tool.or(self.had_tool) };
        Used { tokens: newer.tokens.or(self.tokens), had_tool, model: newer.model.or(self.model) }
    }
}

/// A model's name as a strip row shows it, held inline so that what is
/// known of a context is copied, not allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(in crate::plugins) struct Model {
    len: u8,
    bytes: [u8; 24],
}

impl Model {
    /// `name`, cut to what fits; None for what is no model's name, as a
    /// record of an error has (`<synthetic>`).
    pub fn new(name: &str) -> Option<Model> {
        let name = name.trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)) {
            return None;
        }
        let mut m = Model::default();
        let n = name.len().min(m.bytes.len());
        m.bytes[..n].copy_from_slice(&name.as_bytes()[..n]);
        m.len = n as u8;
        Some(m)
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

/// What is known of a helper's context, and the conversation it is of.
#[derive(Debug, PartialEq)]
pub(in crate::plugins::helpers) struct Known {
    conv: String,
    pub used: Used,
}

impl Shared {
    /// Whether `sid` is a helper whose agent keeps its record where the
    /// plugins look.
    pub(in crate::plugins::helpers) fn wants_used(&self, sid: u64) -> bool {
        self.table().map.get(&sid).is_some_and(|h| !h.contained())
    }

    fn queue_contexts(&self) {
        let any = self.table().map.values().any(|h| h.started.agent == AgentKind::Codex && !h.contained());
        if any && !self.contexts_queued.swap(true, Ordering::AcqRel) && !self.queue(Job::Contexts) {
            self.contexts_queued.store(false, Ordering::Release);
        }
    }

    /// Read what the rollouts of the codex helpers say, each by the
    /// thread its pane is bound to.
    pub(in crate::plugins::helpers) fn look_at_contexts(&self, threads: &HashMap<u64, ThreadView>) {
        self.contexts_queued.store(false, Ordering::Release);
        let table = self.table();
        let mut contexts = self.contexts.lock().unwrap_or_else(|p| p.into_inner());
        contexts.retain(|sid| table.map.contains_key(&sid));
        for h in table.map.values().filter(|h| h.started.agent == AgentKind::Codex && !h.contained()) {
            if let Some(v) = threads.get(&h.sid) {
                let read = contexts.look(h.sid, &v.home, &v.thread, Instant::now());
                self.used(h.sid, &v.thread, read.unwrap_or_default());
            }
        }
    }

    /// The record of conversation `conv`, which helper `sid` is in, says
    /// `read`; nothing said of another conversation is of this one.
    pub(in crate::plugins::helpers) fn used(&self, sid: u64, conv: &str, read: Used) {
        if !self.wants_used(sid) {
            return;
        }
        let mut st = self.lock();
        let same = st.used.get(&sid).filter(|k| k.conv == conv).map(|k| k.used);
        let now = same.unwrap_or_default().then(read);
        if same == Some(now) {
            return;
        }
        let was = st.used.insert(sid, Known { conv: conv.to_string(), used: now });
        drop(st);
        if was.map_or(Used::default(), |k| k.used) == now {
            return;
        }
        self.strip_changed();
        if let Some(h) = crate::pane_control::hub() {
            h.knock();
        }
    }
}

/// Have the worker look at the rollouts of the codex helpers, if there
/// are any.
pub(in crate::plugins) fn look_at_codex() {
    if let Some(s) = SHARED.get() {
        s.queue_contexts();
    }
}

/// Whether pane `sid` is a helper whose context is shown.
pub(in crate::plugins) fn wanted(sid: u64) -> bool {
    SHARED.get().is_some_and(|s| s.wants_used(sid))
}

/// The record of conversation `conv`, of the agent in pane `sid`, says
/// this.
pub(in crate::plugins) fn read(sid: u64, conv: &str, read: Used) {
    if let Some(s) = SHARED.get() {
        s.used(sid, conv, read);
    }
}

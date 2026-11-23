//! Which two panes are a helper and the session that started it.

use super::SHARED;

/// Whether one of two panes is a helper the other started.
pub(in crate::plugins) fn is_pair(a: u64, b: u64) -> bool {
    SHARED.get().is_some_and(|s| s.table().is_pair(a, b))
}

#[cfg(test)]
mod tests {
    use marspot_peer::control::AgentKind;

    use crate::plugins::helpers::table::{Event, Table};
    use crate::plugins::helpers::tests::started;

    #[test]
    fn helpers_only_a_helper_and_its_parent_are_a_pair() {
        let mut t = Table::default();
        for (sid, parent) in [(10, 1), (11, 1), (20, 10), (30, 2)] {
            t.apply(sid, 5, &Event::Started(started(parent, "claude", AgentKind::Codex)));
        }
        for (a, b) in [(10, 1), (11, 1), (20, 10), (30, 2)] {
            assert!(t.is_pair(a, b) && t.is_pair(b, a), "{a} {b}");
        }
        for (a, b) in [(10, 11), (20, 1), (10, 2), (30, 1), (1, 2), (10, 10), (7, 8)] {
            assert!(!t.is_pair(a, b) && !t.is_pair(b, a), "{a} {b}");
        }
        t.apply(10, 6, &Event::Stopped);
        assert!(!t.is_pair(10, 1), "a helper that went is none");
    }
}

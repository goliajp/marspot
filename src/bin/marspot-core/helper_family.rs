//! Whose child each pane is, as the core knows it, and how many
//! children each top pane has.
//!
//! Every pane with a parent counts, shown in a window or not: the
//! shell's strip lists a pane's children by the same rule, and the
//! `+N` and the strip must not disagree.

use std::collections::{HashMap, HashSet};

use marspot_term::session_registry;

/// Panes nest two deep; a chain that loops ends here.
const WALK_MAX: usize = 16;

#[derive(Default)]
pub(crate) struct Family {
    pub(crate) parents: HashMap<u64, u64>,
    /// Each pane with children that is no child itself, and how many.
    pub(crate) counts: Vec<(u64, u32)>,
    /// The counts are those of the panes now.
    pub(crate) counted: bool,
    /// What the panel about to open is of.
    pub(crate) opening: u64,
    /// Shadow panes the panel failed to move to a window, until tried again.
    pub(crate) unmoved: HashSet<u64>,
}

impl Family {
    /// `sid` is the child of `parent` now, or of none (0).
    pub(crate) fn adopted(&mut self, sid: u64, parent: u64) {
        if parent == 0 {
            self.parents.remove(&sid);
        } else {
            self.parents.insert(sid, parent);
        }
    }

    /// The pane at the top of `sid`'s chain of parents.
    pub(crate) fn root(&self, sid: u64) -> u64 {
        let mut at = sid;
        for _ in 0..WALK_MAX {
            match self.parents.get(&at) {
                Some(p) => at = *p,
                None => break,
            }
        }
        at
    }

    pub(crate) fn counted(&self) -> bool {
        self.counted
    }

    pub(crate) fn count(&self, sid: u64) -> u32 {
        self.counts.iter().find(|c| c.0 == sid).map_or(0, |c| c.1)
    }

    /// Count again over the panes now: `hidden` in no window, `shown` in
    /// one.  A pane not adopted yet is looked up in the registry, as
    /// one a core finds at boot is.
    pub(crate) fn recount(&mut self, hidden: &[u64], shown: &[u64]) {
        let all: Vec<u64> = hidden.iter().chain(shown).copied().collect();
        self.parents.retain(|sid, _| all.contains(sid));
        self.unmoved.retain(|sid| hidden.contains(sid));
        for sid in &all {
            if let std::collections::hash_map::Entry::Vacant(v) = self.parents.entry(*sid)
                && let Ok(e) = session_registry::read_session_entry(*sid)
                && e.shadow_of != 0
            {
                v.insert(e.shadow_of);
            }
        }
        self.counts.clear();
        for sid in all {
            let root = self.root(sid);
            if root == sid {
                continue;
            }
            match self.counts.iter_mut().find(|c| c.0 == root) {
                Some(c) => c.1 += 1,
                None => self.counts.push((root, 1)),
            }
        }
        self.counted = true;
    }
}

#[cfg(test)]
#[test]
fn helper_family_counts_every_child_shown_or_not() {
    // nothing below reads the registry: every pane is adopted or a top one
    let _sb = marspot_term::test_env::sandbox_state_dir();
    let mut fam = Family::default();
    fam.adopted(5, 1);
    fam.adopted(6, 1);
    // 5 in no window, 6 shown in one, 7 a top pane in no window
    fam.recount(&[5, 7], &[1, 6]);
    assert_eq!((fam.count(1), fam.count(7), fam.count(5)), (2, 0, 0), "the shown child counts, nobody counts itself");
    fam.adopted(6, 0);
    fam.recount(&[5, 7], &[1, 6]);
    assert_eq!(fam.count(1), 1, "a top pane now");
    fam.recount(&[7], &[1, 6]);
    assert_eq!(fam.count(1), 0, "a child that went is forgotten");
    assert!(!fam.parents.contains_key(&5));
}

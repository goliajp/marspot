//! What L1 last said of the helpers under each pane (`HelperStrip`).
//! Data alone: nothing here draws, lays out or wakes the loop.

use marspot::lx_event;
use marspot::shell_proto::HelperStrip;

/// Panes with a record at once.  Past it the one last heard of longest
/// ago goes, an empty one first.
const CAP: usize = 256;

/// The record of each root pane, the one last heard of at the end.
#[derive(Default)]
pub(super) struct Records {
    list: Vec<HelperStrip>,
    changes: u64,
    all_told: bool,
}

impl Records {
    pub(super) fn take(&mut self, m: HelperStrip) {
        if self.is_news(&m) {
            lx_event!(
                "HELPER_STRIP",
                "L1 said what helpers a pane has",
                session = m.root_sid,
                batch = m.batch,
                rows = m.rows.len()
            );
        }
        self.changes += 1;
        if m.is_all_told() {
            self.all_told = true;
            return;
        }
        self.list.retain(|r| r.root_sid != m.root_sid);
        if self.list.len() == CAP {
            let i = self.list.iter().position(HelperStrip::is_empty).unwrap_or(0);
            self.list.remove(i);
        }
        self.list.push(m);
    }

    /// Another batch, or another number of rows: not each row's progress.
    fn is_news(&self, m: &HelperStrip) -> bool {
        self.of(m.root_sid).is_none_or(|r| (r.batch, r.rows.len()) != (m.batch, m.rows.len()))
    }

    /// The record of pane `root`.  None: L1 has not said; an empty one:
    /// it has no helpers.
    pub(super) fn of(&self, root: u64) -> Option<&HelperStrip> {
        self.list.iter().find(|r| r.root_sid == root)
    }

    pub(super) fn all(&self) -> impl Iterator<Item = &HelperStrip> {
        self.list.iter()
    }

    /// How many were taken.
    pub(super) fn changes(&self) -> u64 {
        self.changes
    }

    pub(super) fn forget(&mut self, root: u64) {
        self.list.retain(|r| r.root_sid != root);
    }

    /// L1 has said everything it had to say at first: a pane with no
    /// record has no helpers.
    pub(super) fn all_told(&self) -> bool {
        self.all_told
    }
}

#[cfg(test)]
mod tests {
    use marspot::shell_proto::{StripAgent, StripRow, StripState};

    use super::*;

    fn with_row(root: u64, batch: u64) -> HelperStrip {
        let row = StripRow {
            sid: 1,
            number: None,
            state: StripState::Working,
            agent: StripAgent::Claude,
            gone: false,
            had_tool: false,
            named: false,
            started_ms: 1,
            worked_ms: 0,
            running_since_ms: Some(1),
            ended_ms: None,
            tokens: None,
            name: String::new(),
            model: String::new(),
            task: String::new(),
        };
        HelperStrip { batch, rows: vec![row], ..HelperStrip::empty(root) }
    }

    #[test]
    fn helper_strip_records_tell_unknown_from_empty() {
        let mut r = Records::default();
        assert_eq!(r.of(7), None);
        r.take(with_row(7, 2));
        r.take(with_row(8, 1));
        assert_eq!(r.of(7).map(|m| m.batch), Some(2));
        r.take(HelperStrip::empty(7));
        assert!(r.of(7).is_some_and(HelperStrip::is_empty));
        assert_eq!(r.of(8).map(|m| m.batch), Some(1));
        assert_eq!(r.list.len(), 2, "one a pane");
    }

    #[test]
    fn helper_strip_records_log_a_batch_or_a_count_that_changed() {
        let mut r = Records::default();
        assert!(r.is_news(&with_row(7, 2)));
        r.take(with_row(7, 2));
        let mut m = with_row(7, 2);
        m.rows[0].tokens = Some(5);
        assert!(!r.is_news(&m), "a row moved on");
        assert!(r.is_news(&with_row(7, 3)) && r.is_news(&HelperStrip::empty(7)));
    }

    #[test]
    fn helper_strip_records_end_marker_is_no_panes_record() {
        let mut r = Records::default();
        r.take(with_row(7, 1));
        assert!(!r.all_told());
        r.take(HelperStrip::all_told());
        assert!(r.all_told() && r.of(0).is_none() && r.of(7).is_some());
    }

    #[test]
    fn helper_strip_records_are_bounded_and_drop_empty_ones_first() {
        let mut r = Records::default();
        for root in 1..=CAP as u64 {
            r.take(if root == 5 { HelperStrip::empty(root) } else { with_row(root, 1) });
        }
        r.take(with_row(1000, 1));
        assert_eq!(r.list.len(), CAP);
        assert!(r.of(5).is_none() && r.of(1).is_some() && r.of(1000).is_some());
        r.take(with_row(1001, 1));
        assert!(r.of(1).is_none() && r.of(2).is_some(), "then the oldest");
    }
}

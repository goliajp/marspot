//! What the shell remembers per pane, outside the tables that prune
//! themselves (`PaneLook`, the pane-state machines, the op queue).
//!
//! Every field is keyed by session id, and a session the registry no
//! longer lists is a closed pane: `retain` is where all of them forget
//! it, so none grows by one entry per pane ever opened.

use std::collections::HashMap;

use crate::plugins::autorun::Memory;
use crate::plugins::host::PaneWheelKeysUpdate;

#[derive(Default)]
pub struct PaneTables {
    /// The wheel declarations currently in force, by pane.
    ///
    /// A wheel declaration is issued ONCE per pane — a plugin says "this is
    /// how the wheel reaches me" and has nothing to repeat.  Dropping
    /// that one frame left the pane with no wheel keys for the rest of
    /// its life, and it landed exactly in the gap a core swap opens
    /// (2026-09-06: codex declared 1s after L1 re-execed, core was
    /// mid-boot, the wheel then did nothing at all).  So the shell
    /// keeps the mapping and replays it on every handshake.
    pub wheel_keys: HashMap<u64, PaneWheelKeysUpdate>,
    /// Last agent-TUI declaration per pane, so the heartbeat is
    /// only logged when it changes.
    pub agent_tui: HashMap<u64, bool>,
    /// The markup declarations in force, replayed on every handshake
    /// for the same reason the wheel ones are: a plugin says it once.
    pub render_markup: HashMap<u64, bool>,
    /// Per-pane memory for the autorun policy.
    pub autorun_mem: HashMap<u64, Memory>,
    /// The last autorun reason logged for each pane, so a steady state
    /// is one line rather than one every thirty seconds.
    pub autorun_why: HashMap<u64, String>,
}

impl PaneTables {
    /// Forget every pane `open` rejects.
    pub fn retain(&mut self, open: impl Fn(u64) -> bool) {
        self.wheel_keys.retain(|sid, _| open(*sid));
        self.agent_tui.retain(|sid, _| open(*sid));
        self.render_markup.retain(|sid, _| open(*sid));
        self.autorun_mem.retain(|sid, _| open(*sid));
        self.autorun_why.retain(|sid, _| open(*sid));
    }

    #[cfg(test)]
    fn holds(&self, sid: u64) -> Vec<&'static str> {
        [
            ("wheel_keys", self.wheel_keys.contains_key(&sid)),
            ("agent_tui", self.agent_tui.contains_key(&sid)),
            ("render_markup", self.render_markup.contains_key(&sid)),
            ("autorun_mem", self.autorun_mem.contains_key(&sid)),
            ("autorun_why", self.autorun_why.contains_key(&sid)),
        ]
        .into_iter()
        .filter_map(|(name, held)| held.then_some(name))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(t: &mut PaneTables, sid: u64) {
        t.wheel_keys.insert(
            sid,
            PaneWheelKeysUpdate {
                shelld_session_id: sid,
                enter: Vec::new(),
                up: b"k".to_vec(),
                down: b"j".to_vec(),
                marker: Vec::new(),
            },
        );
        t.agent_tui.insert(sid, true);
        t.render_markup.insert(sid, true);
        t.autorun_mem.insert(sid, Memory::default());
        t.autorun_why.insert(sid, "waiting".into());
    }

    #[test]
    fn a_closed_pane_is_dropped_from_every_table_and_an_open_one_kept() {
        let mut t = PaneTables::default();
        fill(&mut t, 1);
        fill(&mut t, 2);
        assert_eq!(t.holds(1).len(), 5, "the test fills every table");
        t.retain(|sid| sid == 2);
        assert!(t.holds(1).is_empty(), "still held: {:?}", t.holds(1));
        assert_eq!(t.holds(2).len(), 5);
    }
}

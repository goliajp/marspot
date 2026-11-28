//! The rows of a pane's menu about whose child it is, and their picks.
//!
//! A pick goes to the shell as the helper panel's `SetParent`, so the
//! one place that checks the two-level rule decides it, and its
//! `PanePlace` comes back to the core to be carried out.

use marspot::shell_proto::{CwdControl, HelperPanelAct, PanelAct, encode_helper_panel_act};
use marspot::ui::components::MenuItem;

use super::menus::{Kin, kin_items};
use super::*;

/// The row that opens the panes a pane can belong to; never picked.
pub(crate) const BELONGS_TAG: u32 = 0x5100_0000;
/// The row that makes a child a top-level pane.
pub(crate) const MAKE_TOP_TAG: u32 = 0x5100_0001;
/// A pane to belong to: this plus its place in the menu's pane list.
pub(crate) const PARENT_TAG_BASE: u32 = 0x5200_0000;

/// Panes a menu lists at most.
const LISTED_CAP: usize = crate::draw::sidebar_tree::ROW_CAP;

impl CoreApp {
    /// The rows about whose child `sid` is.  The panes it can belong
    /// to are kept for the pick: any top-level pane but itself whose
    /// session is still there.
    pub(crate) fn kin_of(&mut self, sid: u64) -> Vec<MenuItem> {
        if !self.pane_control.shadows.family.counted() {
            self.shadows_changed();
        }
        let fam = &self.pane_control.shadows.family;
        if fam.parents.contains_key(&sid) {
            return kin_items(Kin::Child);
        }
        if fam.count(sid) > 0 {
            return kin_items(Kin::Parent);
        }
        let all = self.windows.iter().flat_map(|w| w.panes.iter()).chain(&self.shadows);
        let listed: Vec<u64> = all
            // a session that ended is going: a slot still starting is not
            .filter(|p| !p.is_dormant() && !(p.is_l3() && p.is_exited()))
            .filter_map(Pane::shelld_session_id)
            .filter(|c| *c != sid && !fam.parents.contains_key(c))
            .take(LISTED_CAP)
            .collect();
        let names = self.pane_names();
        let rows = listed
            .iter()
            .enumerate()
            .map(|(i, c)| MenuItem::entry(&self.coded_name(*c, &names), PARENT_TAG_BASE + i as u32))
            .collect();
        self.pane_control.shadows.menu_panes = listed;
        kin_items(Kin::Top(rows))
    }

    /// `#4145 smix`: a pane's code and name, as a menu lists it.
    pub(crate) fn coded_name(&self, sid: u64, names: &std::collections::HashMap<u64, String>) -> String {
        let name = names.get(&sid).map_or("", String::as_str);
        match marspot_term::pane_code::known(sid) {
            Some(code) => format!("#{code:04} {}", crate::draw::labels::without_code(name, code)),
            None => name.to_string(),
        }
    }

    /// The pane a menu opened on `region` is about.
    pub(crate) fn region_sid(&self, wi: usize, region: ContextRegion) -> Option<u64> {
        match region {
            ContextRegion::SidebarSlot(i) | ContextRegion::Pane(i) => win!(self, wi).panes.get(i)?.shelld_session_id(),
            ContextRegion::SidebarPane(sid) => Some(sid),
            _ => None,
        }
    }

    /// A row about whose child a pane is; false when `tag` is none.
    pub(crate) fn pick_kin(&mut self, wi: usize, region: ContextRegion, tag: u32) -> bool {
        let parent = match tag {
            MAKE_TOP_TAG => Some(0),
            t if (PARENT_TAG_BASE..PARENT_TAG_BASE + LISTED_CAP as u32).contains(&t) => {
                self.pane_control.shadows.menu_panes.get((t - PARENT_TAG_BASE) as usize).copied()
            }
            BELONGS_TAG => None,
            _ => return false,
        };
        win!(self, wi).context_menu = None;
        win!(self, wi).needs_render = true;
        if let (Some(parent), Some(sid)) = (parent, self.region_sid(wi, region)) {
            let act = HelperPanelAct { sid, act: PanelAct::SetParent, want: CwdControl::Flip, parent };
            self.pending_to_shell.push((MsgType::HelperPanelAct, encode_helper_panel_act(&act)));
            lx_event!("PANE_PARENT_ASKED", "the user asked from a menu whose child a pane is", session = sid, parent = parent);
        }
        true
    }
}

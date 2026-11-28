//! How a sidebar control's look is packed into its node's two words of
//! data, and folded into the sidebar's fingerprint.

use marspot_term::layout::Rect;
use marspot_term::render::SidebarTree;
use marspot_term::session::SessionState;

/// `data[0]` of a row: it is the pane in use.
pub(super) const SELECTED: u32 = 1;
/// `data[0]` of anything: the pointer is not to light it, because a
/// panel or a menu lies over the window.
pub(super) const QUIET: u32 = 2;
/// `data[0]` of the add button: no more panes can be added.
pub(super) const FULL: u32 = 4;
/// `data[0]` of a row: its pane is not the last, so it can be closed.
pub(super) const CAN_CLOSE: u32 = 8;
/// `data[0]` of a row, from this bit up: its session's state.
pub(super) const STATE_SHIFT: u32 = 4;
/// `data[1]` of a row: its low 16 bits are the pane's code.
pub(super) const HAS_CODE: u32 = 1 << 16;
/// `data[1]` of a row: its pane is in no window.
pub(super) const HOLLOW: u32 = 1 << 17;
/// `data[1]` of a row, from this bit up: where it sits in the tree.
pub(super) const TREE_SHIFT: u32 = 18;

pub(super) fn tree_bits(t: SidebarTree) -> u32 {
    match t {
        SidebarTree::Top => 0,
        SidebarTree::Child => 1,
        SidebarTree::LastChild => 2,
        SidebarTree::Parent => 3,
    }
}

pub(super) fn tree_of(bits: u32) -> SidebarTree {
    match bits & 3 {
        0 => SidebarTree::Top,
        1 => SidebarTree::Child,
        2 => SidebarTree::LastChild,
        _ => SidebarTree::Parent,
    }
}

pub(super) fn rect_bits(r: &Rect) -> [u64; 4] {
    [r.x.to_bits(), r.y_top.to_bits(), r.w.to_bits(), r.h.to_bits()]
}

pub(super) fn state_bits(s: SessionState) -> u32 {
    match s {
        SessionState::Active => 0,
        SessionState::Idle => 1,
        SessionState::Exited => 2,
    }
}

pub(super) fn state_of(bits: u32) -> SessionState {
    match bits {
        0 => SessionState::Active,
        1 => SessionState::Idle,
        _ => SessionState::Exited,
    }
}

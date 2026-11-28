//! The question before closing a pane with child panes, over the pane,
//! with its two buttons, at both display scales.  Baselines
//! `bench/ui/snapshots/chrome_close_ask{,.2x}.png`.

use super::super::density_kit;
use super::super::pane_notice::ask_text;
use super::{pane, snapshot, PaneSpec};
use crate::session::SessionState;
use crate::ui::strings::{t, Msg};

#[test]
pub(super) fn chrome_snapshot_close_ask() {
    let body = format!("{}\n#8590 madpdf\n#7001 helper-a", t(Msg::CloseChildrenAsk));
    let ask = ask_text(&body, t(Msg::CloseChildrenConfirm), t(Msg::CloseChildrenCancel));
    let panes = [PaneSpec { notice: Some(ask), ..pane("devops", "", SessionState::Idle) }, pane("smix", "", SessionState::Active)];
    snapshot("chrome_close_ask", (1200, 640), (2, 1), &panes, 1, |_, _, _| {});
}

#[test]
fn chrome_snapshot_close_ask_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_close_ask);
}

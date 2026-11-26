//! The accounts panel with stored resets listed under the bars.

use super::panel_snapshot_tests::{accounts_data, snapshot};
use crate::layout::Layout;
use crate::render_metal::MetalRenderer;
use crate::ui::components::cc_usage_modal::{self, CcUsageRender};

/// Two Codex accounts with credits, a Claude account with two kinds of
/// reset and one the provider says cannot have any.
pub(super) fn accounts_with_resets(renderer: &mut MetalRenderer, layout: &Layout) {
    let (mut accounts, updated_label, clock) = accounts_data();
    let lines = |l: &[&str]| l.iter().map(|s| crate::agent_resets::ResetLine { text: s.to_string(), ..Default::default() }).collect::<Vec<_>>();
    accounts[0].resets = lines(&["full 2/3 to 12/1 05:26 from 11/28 09:00", "fable 1/1 to 12/5 09:00 not now"]);
    accounts[2].resets = lines(&["resets: ineligible (surface)"]);
    accounts[8].resets = lines(&["full 4/4 to 11/30 05:26"]);
    accounts[9].resets = lines(&["full 3/3 to 12/1 07:31"]);
    let (cell_w, cell_h) = renderer.cell_dims();
    let reset_rows = accounts.iter().map(|a| a.resets.len()).max().unwrap_or(0);
    let rect = cc_usage_modal::panel_rect_with(
        (accounts.len(), 2, reset_rows), layout.window_w, layout.window_h, cell_w, cell_h, layout.top_inset,
    );
    renderer.set_cc_usage(Some(CcUsageRender { rect, updated_label, accounts, clock, pointer: None, surface: Default::default() }));
}

/// Wide enough for the ten cards in one row.
#[test]
pub(super) fn panel_snapshot_accounts_resets() {
    snapshot("accounts_resets", 2800, 1400, accounts_with_resets);
}

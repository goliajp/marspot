//! The panels and the chrome again, in Japanese and in Chinese.
//!
//! What a translation breaks is not in the catalogue: a label wider
//! than its control, a header that no longer sits over its column, a
//! Han character drawn in the other language's form.  These are the
//! pictures that show it.

use super::chrome_snapshot_tests::{chrome_snapshot_menu, chrome_snapshot_search, chrome_snapshot_window};
use super::panel_snapshot_tests::{
    panel_snapshot_accounts, panel_snapshot_accounts_empty, panel_snapshot_controls, panel_snapshot_layout,
    panel_snapshot_process, panel_snapshot_settings, panel_snapshot_shortcuts, snapshot,
};
use crate::ui::components::panel_palette as palette;
use crate::ui::core::ViewPainter;

/// Three characters whose Japanese and Chinese forms differ, drawn the
/// two ways the app draws text: shaped, as a panel does, and cell by
/// cell, as the terminal and the badges do.
fn han_forms_sheet(p: &mut ViewPainter<'_>) {
    const PROBE: &str = "直骨海";
    let sheet = marspot_term::layout::Rect { x: 0.0, y_top: 0.0, w: p.window_w, h: p.window_h };
    p.fill_rounded_rect(sheet, palette::surface_base(), 0.0, (palette::none(), 0.0));
    p.ui_text_at(40.0, 150.0, PROBE, 56.0, 400, palette::fg());
    p.text(40.0, 240.0, PROBE, palette::fg());
}

fn han_forms() {
    snapshot("han_forms", 520, 300, |_, _| {
        *crate::frame_build::frame::TEST_OVERLAY.lock().unwrap() = Some(han_forms_sheet);
    });
}

crate::snapshot_kit::snapshots_in_other_languages! {
    panel_snapshot_settings_ja, panel_snapshot_settings_zh = panel_snapshot_settings;
    panel_snapshot_shortcuts_ja, panel_snapshot_shortcuts_zh = panel_snapshot_shortcuts;
    panel_snapshot_layout_ja, panel_snapshot_layout_zh = panel_snapshot_layout;
    panel_snapshot_process_ja, panel_snapshot_process_zh = panel_snapshot_process;
    panel_snapshot_accounts_ja, panel_snapshot_accounts_zh = panel_snapshot_accounts;
    panel_snapshot_accounts_empty_ja, panel_snapshot_accounts_empty_zh = panel_snapshot_accounts_empty;
    panel_snapshot_controls_ja, panel_snapshot_controls_zh = panel_snapshot_controls;
    chrome_snapshot_menu_ja, chrome_snapshot_menu_zh = chrome_snapshot_menu;
    chrome_snapshot_window_ja, chrome_snapshot_window_zh = chrome_snapshot_window;
    chrome_snapshot_search_ja, chrome_snapshot_search_zh = chrome_snapshot_search;
    panel_snapshot_han_forms_ja, panel_snapshot_han_forms_zh = han_forms;
}

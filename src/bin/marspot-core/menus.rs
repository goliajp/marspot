//! What each context menu offers, as groups of rows.  The menu itself
//! is `grouped`'s: a divider only between two groups that have rows.

use super::*;
use marspot::commands::{Command, hint};
use marspot::ui::components::{MenuItem, grouped};

/// The cross-window rows of a pane's menu: "Move to Window N" for every
/// window but `wi` (1-based creation order), and "Move to New Window",
/// greyed out when it would only rebuild the one window with its one pane.
pub(crate) fn move_items(windows: usize, wi: usize, panes_here: usize) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = (0..windows)
        .filter(|ti| *ti != wi)
        .map(|ti| MenuItem::entry(&strings::text(Arg::MoveToWindow(ti + 1)), MOVE_TO_WINDOW_TAG_BASE + ti as u32))
        .collect();
    let pointless = windows == 1 && panes_here <= 1;
    let mi = MenuItem::entry(t(Msg::MenuMoveToNewWindow), MOVE_TO_NEW_WINDOW_TAG);
    items.push(if pointless { mi.disabled() } else { mi });
    items
}

/// The menu of a pane's terminal.  Close is never disabled: closing a
/// window's last pane closes the window, the last window's quits.
pub(super) fn pane_menu(has_selection: bool, moves: Vec<MenuItem>) -> Vec<MenuItem> {
    let copy = MenuItem::entry(t(Msg::MenuCopy), ContextMenuAction::CopySelection.tag()).with_shortcut(&hint(Command::Copy));
    grouped([
        vec![
            if has_selection { copy } else { copy.disabled() },
            MenuItem::entry(t(Msg::MenuPaste), ContextMenuAction::Paste.tag()).with_shortcut(&hint(Command::Paste)),
        ],
        vec![MenuItem::entry(t(Msg::MenuClearScrollback), ContextMenuAction::ClearScrollback.tag())],
        vec![
            MenuItem::entry(t(Msg::MenuNewPane), ContextMenuAction::SplitNewPane.tag()),
            MenuItem::entry(t(Msg::MenuClosePane), ContextMenuAction::ClosePane.tag()),
        ],
        moves,
    ])
}

/// The menu of a sidebar row.
pub(super) fn sidebar_menu(moves: Vec<MenuItem>) -> Vec<MenuItem> {
    grouped([vec![MenuItem::entry(t(Msg::MenuClosePane), ContextMenuAction::ClosePane.tag())], moves])
}

/// The menu of the title strip.
pub(super) fn title_menu() -> Vec<MenuItem> {
    grouped([vec![
        MenuItem::entry(t(Msg::MenuToggleSidebar), ContextMenuAction::ToggleSidebar.tag())
            .with_shortcut(&hint(Command::ToggleSidebar)),
        MenuItem::entry(t(Msg::MenuOpenLayout), ContextMenuAction::OpenLayout.tag()),
    ]])
}

/// The menu of a badge: the rows its plugin sent, then the helpers of
/// the pane (`helpers` of them) and the way back from viewing one.
pub(super) fn badge_menu(plugin: Vec<marspot::shell_proto::PaneBadgeMenuItem>, helpers: u32, viewing: bool) -> Vec<MenuItem> {
    let of_helpers = (helpers > 0).then(|| MenuItem::entry(&strings::text(Arg::HelpersOf(helpers)), helper_panel::MENU_TAG));
    let back = viewing.then(|| MenuItem::entry(t(Msg::HelperViewBack), helper_panel::BACK_TAG));
    grouped([plugin.into_iter().map(|it| MenuItem::entry(&it.label, it.tag)).chain(of_helpers).chain(back).collect::<Vec<_>>()])
}

#[cfg(test)]
#[path = "menus_tests.rs"]
mod tests;

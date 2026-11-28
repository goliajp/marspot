//! Every context menu, in every combination of the rows it can show,
//! neither opens nor ends with a divider and never has two that meet.

use super::*;
use marspot::grid_links::LinkKind;
use marspot::ui::components::dividers_sound;

fn check(what: &str, items: &[MenuItem]) {
    let shown: Vec<&str> = items.iter().map(|i| if i.divider { "—" } else { i.label.as_str() }).collect();
    assert!(dividers_sound(items), "{what}: {shown:?}");
}

fn all_moves() -> Vec<(String, Vec<MenuItem>)> {
    let mut out = Vec::new();
    for n in 1..=4usize {
        for wi in 0..n {
            for p in 0..=2usize {
                out.push((format!("{n} windows, in {wi}, {p} panes"), move_items(n, wi, p)));
            }
        }
    }
    out
}

#[test]
fn menus_have_no_stray_dividers_in_any_combination() {
    for (what, moves) in all_moves() {
        for sel in [false, true] {
            check(&format!("pane, selection {sel}, {what}"), &pane_menu(sel, moves.clone()));
        }
        for kin in [Kin::Top(Vec::new()), Kin::Top(vec![MenuItem::entry("#4145 smix", 1)]), Kin::Parent, Kin::Child] {
            check(&format!("sidebar, {what}"), &sidebar_menu(kin_items(kin), moves.clone()));
        }
    }
    check("sidebar, no moves", &sidebar_menu(Vec::new(), Vec::new()));
    check("pane, no moves", &pane_menu(false, Vec::new()));
    check("title", &title_menu());
    for n in 0..3 {
        let plugin: Vec<_> =
            (0..n).map(|i| marspot::shell_proto::PaneBadgeMenuItem { label: format!("row {i}"), tag: i }).collect();
        for helpers in [0, 2] {
            for viewing in [false, true] {
                check(&format!("badge {n} {helpers} {viewing}"), &badge_menu(plugin.clone(), helpers, viewing));
            }
        }
    }
    for kind in [LinkKind::Url, LinkKind::File, LinkKind::Email, LinkKind::Ip, LinkKind::Uuid] {
        for declared in [false, true] {
            let l = LinkContext { text: "https://example.com/a".into(), target: None, kind, declared };
            check(&format!("link {kind:?} {declared}"), &link_menu_items_for(&l));
        }
    }
}

/// The sidebar row's menu starts with its first row, not a divider.
#[test]
fn the_sidebar_menu_opens_with_close() {
    let m = sidebar_menu(kin_items(Kin::Child), move_items(2, 0, 3));
    assert_eq!(m[0].action_tag, ContextMenuAction::ClosePane.tag());
    assert!(m[1].divider && !m[2].divider);
}

/// A pane with children may not belong to another: the row is there,
/// off, and says why; a top-level pane with none to belong to has the
/// row off; a child is offered the way back to the top.
#[test]
fn the_kin_rows_say_what_whose_child_a_pane_can_be() {
    let parent = kin_items(Kin::Parent);
    assert!(!parent[0].enabled && parent[0].tooltip == t(Msg::MenuBelongsToBlocked));
    let lonely = kin_items(Kin::Top(Vec::new()));
    assert!(!lonely[0].enabled && lonely[0].tooltip.is_empty() && lonely[0].submenu.is_empty());
    let top = kin_items(Kin::Top(vec![MenuItem::entry("#4145 smix", 7)]));
    assert!(top[0].opens() && top[0].label == t(Msg::MenuBelongsTo));
    assert_eq!(kin_items(Kin::Child)[0].action_tag, super::menu_family::MAKE_TOP_TAG);
}

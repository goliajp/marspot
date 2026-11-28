//! The sidebar as a tree of panes, at both display scales: children
//! indented under their parent with a tree line, a hollow dot for a
//! pane in no window, a parent of another window as a grey row above
//! its child shown here, and a long name cut with its code whole.
//! Baselines `bench/ui/snapshots/chrome_sidebar_tree{,.2x}.png`, and
//! `chrome_sidebar_minimized{,.2x}.png` beside them.

use super::super::density_kit;
use super::{pane, snapshot, ROWS};
use crate::render::{SidebarEntry, SidebarTree};
use crate::session::SessionState;

fn row(label: &'static str, state: SessionState, code: u16, tree: SidebarTree, hollow: bool) -> SidebarEntry<'static> {
    SidebarEntry { tree, hollow, ..SidebarEntry::top(label, state, Some(code)) }
}

/// Four panes in the window; devops has a child in another window and
/// one in none, alt is the child of a pane of another window.
#[test]
pub(super) fn chrome_snapshot_sidebar_tree() {
    use SessionState::{Active, Exited, Idle};
    use SidebarTree::{Child, LastChild, Parent, Top};
    let rows = vec![
        SidebarEntry { selected: true, ..row("smix", Active, 4145, Top, false) },
        row("devops", Idle, 5528, Top, false),
        row("helper-with-a-name-longer-than-any-row-can-hold", Active, 7001, Child, false),
        row("madpdf", Idle, 8590, LastChild, true),
        row("insight-mono", Exited, 8654, Top, false),
        row("lab", Idle, 2210, Parent, false),
        row("alt", Idle, 6004, LastChild, false),
    ];
    let panes = [
        pane("smix", "", Active),
        pane("devops", "", Idle),
        pane("insight-mono", "", Exited),
        pane("alt", "", Idle),
    ];
    ROWS.with(|r| *r.borrow_mut() = Some(rows));
    snapshot("chrome_sidebar_tree", (1600, 1000), (2, 2), &panes, 0, |_, _, _| {});
}

#[test]
fn chrome_snapshot_sidebar_tree_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_sidebar_tree);
}

/// Another window is minimized: the parent it shows, a grey row here,
/// and the child it shows are drawn hollow, the panes of this window
/// solid.  Baselines `chrome_sidebar_minimized{,.2x}.png`.
#[test]
pub(super) fn chrome_snapshot_sidebar_minimized() {
    use SessionState::{Active, Idle};
    use SidebarTree::{LastChild, Parent, Top};
    let rows = vec![
        row("lab", Idle, 2210, Parent, true),
        SidebarEntry { selected: true, ..row("smix", Active, 4145, LastChild, false) },
        row("devops", Idle, 5528, Top, false),
        row("madpdf", Active, 8590, LastChild, true),
    ];
    let panes = [pane("smix", "", Active), pane("devops", "", Idle)];
    ROWS.with(|r| *r.borrow_mut() = Some(rows));
    snapshot("chrome_sidebar_minimized", (1200, 640), (2, 1), &panes, 0, |_, _, _| {});
}

#[test]
fn chrome_snapshot_sidebar_minimized_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_sidebar_minimized);
}

/// The sidebar of a 4×1 window holding seven panes: the four drawn
/// solid, the three in slots past the grid's cells hollow.
fn undrawn_rows() -> Vec<SidebarEntry<'static>> {
    use SessionState::{Active, Exited, Idle};
    use SidebarTree::Top;
    vec![
        SidebarEntry { selected: true, ..row("smix", Active, 4145, Top, false) },
        row("devops", Idle, 5528, Top, false),
        row("insight-mono", Exited, 8654, Top, false),
        row("alt", Idle, 6004, Top, false),
        row("madpdf", Idle, 8590, Top, true),
        row("madpdf", Active, 7009, Top, true),
        row("alt-registry", Idle, 9898, Top, true),
    ]
}

fn undrawn_panes() -> [super::PaneSpec; 4] {
    use SessionState::{Active, Exited, Idle};
    [pane("smix", "", Active), pane("devops", "", Idle), pane("insight-mono", "", Exited), pane("alt", "", Idle)]
}

/// Baselines `chrome_sidebar_undrawn{,.2x}.png`.
#[test]
pub(super) fn chrome_snapshot_sidebar_undrawn() {
    ROWS.with(|r| *r.borrow_mut() = Some(undrawn_rows()));
    snapshot("chrome_sidebar_undrawn", (1600, 1000), (4, 1), &undrawn_panes(), 0, |_, _, _| {});
}

#[test]
fn chrome_snapshot_sidebar_undrawn_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_sidebar_undrawn);
}

/// At 1× a hollow dot has a hole the eye cannot miss: a run of pixels
/// of the sidebar's own colour across its middle, inside a ring near
/// as bright as a solid dot of the same state.
#[test]
fn sidebar_hollow_dot_shows_a_clear_hole_at_1x() {
    use crate::ui::components::sidebar::{status_dot, Sidebar};
    use marspot_term::layout::Rect;
    let Ok(mut renderer) = crate::render_metal::MetalRenderer::new_headless() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let (rgba, (w, _), l) = density_kit::at_scale(1.0, || {
        ROWS.with(|r| *r.borrow_mut() = Some(undrawn_rows()));
        let mut wr = crate::render_metal::WindowRender::new();
        super::scene_rgba(&mut renderer, &mut wr, None, (1600, 1000), (4, 1), &undrawn_panes(), 0, |_, _, _| {})
            .expect("a frame")
    });
    let at = |x: f64, y: f64| -> [i32; 3] {
        let i = ((y as usize) * w as usize + x as usize) * 4;
        [rgba[i] as i32, rgba[i + 1] as i32, rgba[i + 2] as i32]
    };
    let lum = |p: [i32; 3]| p[0] + p[1] + p[2];
    let near = |a: [i32; 3], b: [i32; 3]| (0..3).all(|c| (a[c] - b[c]).abs() <= 8);
    let column = Sidebar {
        rect: Rect { x: 0.0, y_top: l.top_inset, w: l.sidebar_w, h: l.window_h - l.top_inset },
        top_pad: l.sidebar_top_pad_phys,
    };
    let middle = |i: usize| {
        let d = status_dot(column.row_rect(i), SidebarTree::Top);
        (d.x + d.w * 0.5, d.y_top + d.h * 0.5)
    };
    // alt (row 3) is idle and solid; madpdf (row 4) idle and hollow,
    // alt-registry (row 6) too
    let (sx, sy) = middle(3);
    let solid = at(sx, sy);
    let bg = at(sx - 10.0, sy);
    assert!(!near(solid, bg), "the solid dot is drawn: {solid:?} on {bg:?}");
    for i in [4, 6] {
        let (cx, cy) = middle(i);
        let hole = (-6..6).filter(|dx| near(at(cx + *dx as f64, cy), bg)).count();
        assert!(hole >= 6, "row {i}: {hole} px of the sidebar's colour across the hole, want 6 or more");
        assert!(near(at(cx, cy), bg) && near(at(cx - 1.0, cy - 1.0), bg), "row {i}: the middle is open");
        let ring = (-8..8).map(|dx| lum(at(cx + dx as f64, cy))).max().unwrap_or(0);
        let bright = ring - lum(bg) >= (lum(solid) - lum(bg)) * 6 / 10;
        assert!(bright, "row {i}: the ring ({ring}) is near as bright as the dot ({}) on {}", lum(solid), lum(bg));
    }
}

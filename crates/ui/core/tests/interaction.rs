//! The pointer and the keyboard, on plain data: a tree, its layout and
//! an interaction record.  No window, no fonts.

use golia_ui_core::{
    Action, Align, Constraints, FakeMeasure, GroupKind, Interaction, Key, KeyResult, Layout,
    Length as L, Metrics, NO_ITEM, Target, Tree, Ui, WidgetId, hit_test,
};

const METRICS: Metrics = Metrics { px_per_pt: 1.0, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 };
const MEASURE: FakeMeasure = FakeMeasure::new(1.0);

const A: WidgetId = WidgetId(1);
const B: WidgetId = WidgetId(2);
const SEG: WidgetId = WidgetId(3);
const LIST: WidgetId = WidgetId(4);
const C: WidgetId = WidgetId(5);

fn lay(f: impl FnOnce(&mut Ui<'_>)) -> (Tree, Layout) {
    let mut tree = Tree::new();
    tree.build(f);
    let mut layout = Layout::new();
    layout.run(&tree, (0.0, 0.0), Constraints::loose(400.0, 400.0), &METRICS, &MEASURE);
    (tree, layout)
}

/// A button, a disabled button, a three-way segmented control with
/// its middle segment chosen, a three-row list with its middle row
/// disabled, and a last button.  30 px a row, the list rows 20.
///
/// ```text
///   0  A
///  30  B (disabled)
///  60  SEG: [0..40) [40..80) [80..120)
///  90  LIST row 0
/// 110  LIST row 1 (disabled)
/// 130  LIST row 2
/// 150  C
/// ```
fn form() -> (Tree, Layout) {
    lay(|ui| {
        ui.vstack(|ui| {
            ui.leaf(1, L::Px(100.0), L::Px(30.0)).widget(A, Action::new(10, 0));
            ui.leaf(1, L::Px(100.0), L::Px(30.0)).widget(B, Action::new(20, 0)).disabled(true);
            ui.group(SEG, GroupKind::Segmented, false, |ui| {
                for k in 0..3 {
                    ui.leaf(0, L::Px(40.0), L::Px(30.0)).item(Action::new(30, k)).current(k == 1);
                }
            });
            ui.group(LIST, GroupKind::List, true, |ui| {
                for k in 0..3 {
                    ui.leaf(0, L::Px(100.0), L::Px(20.0)).item(Action::new(40, k)).disabled(k == 1);
                }
            });
            ui.leaf(1, L::Px(100.0), L::Px(30.0)).widget(C, Action::new(50, 0));
        });
    })
}

fn on(id: WidgetId) -> Option<Target> {
    Some(Target { id, item: NO_ITEM })
}

fn item(id: WidgetId, item: u32) -> Option<Target> {
    Some(Target { id, item })
}

#[test]
fn tab_goes_through_the_stops_in_the_order_they_were_described_and_round_again() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    assert_eq!(s.focused, None, "nothing has the focus until a key asks");

    let mut seen = Vec::new();
    for _ in 0..5 {
        assert_eq!(s.key(&tree, &layout, Key::Tab), KeyResult::Handled);
        seen.push(s.focused);
    }
    assert_eq!(
        seen,
        [
            on(A),
            // B is disabled; the group is entered on its chosen segment
            item(SEG, 1),
            item(LIST, 0),
            on(C),
            // and round
            on(A),
        ]
    );
}

#[test]
fn shift_tab_goes_the_other_way_and_wraps_too() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    s.key(&tree, &layout, Key::BackTab);
    assert_eq!(s.focused, on(C), "from nothing, backwards starts at the end");
    s.key(&tree, &layout, Key::BackTab);
    assert_eq!(s.focused, item(LIST, 0));
    s.key(&tree, &layout, Key::BackTab);
    assert_eq!(s.focused, item(SEG, 1));
    s.key(&tree, &layout, Key::BackTab);
    assert_eq!(s.focused, on(A), "B is skipped backwards as well");
    s.key(&tree, &layout, Key::BackTab);
    assert_eq!(s.focused, on(C));
}

#[test]
fn a_surface_with_nothing_to_focus_has_no_use_for_tab() {
    let (tree, layout) = lay(|ui| {
        ui.vstack(|ui| {
            ui.leaf(0, L::Px(10.0), L::Px(10.0));
        });
    });
    let mut s = Interaction::default();
    assert_eq!(s.key(&tree, &layout, Key::Tab), KeyResult::Ignored);
    assert_eq!(s.key(&tree, &layout, Key::Enter), KeyResult::Ignored);
}

#[test]
fn arrows_move_inside_a_segmented_control_choose_as_they_go_and_stop_at_its_ends() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    s.key(&tree, &layout, Key::Tab);
    s.key(&tree, &layout, Key::Tab);
    assert_eq!(s.focused, item(SEG, 1));

    assert_eq!(s.key(&tree, &layout, Key::Right), KeyResult::Action(Action::new(30, 2)));
    assert_eq!(s.focused, item(SEG, 2));
    assert_eq!(s.key(&tree, &layout, Key::Right), KeyResult::Handled, "the end: spent, not passed on");
    assert_eq!(s.focused, item(SEG, 2), "and the focus has not left the control");

    assert_eq!(s.key(&tree, &layout, Key::Left), KeyResult::Action(Action::new(30, 1)));
    assert_eq!(s.key(&tree, &layout, Key::Left), KeyResult::Action(Action::new(30, 0)));
    assert_eq!(s.key(&tree, &layout, Key::Left), KeyResult::Handled);
    assert_eq!(s.focused, item(SEG, 0));

    assert_eq!(s.key(&tree, &layout, Key::Down), KeyResult::Ignored, "not this control's axis");
    assert_eq!(s.focused, item(SEG, 0));
}

#[test]
fn arrows_move_inside_a_list_skip_a_disabled_row_and_enter_triggers() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    for _ in 0..3 {
        s.key(&tree, &layout, Key::Tab);
    }
    assert_eq!(s.focused, item(LIST, 0));

    assert_eq!(s.key(&tree, &layout, Key::Down), KeyResult::Handled, "moving does not trigger");
    assert_eq!(s.focused, item(LIST, 2), "row 1 is disabled");
    assert_eq!(s.key(&tree, &layout, Key::Down), KeyResult::Handled);
    assert_eq!(s.focused, item(LIST, 2), "the last row is the end");
    assert_eq!(s.key(&tree, &layout, Key::Enter), KeyResult::Action(Action::new(40, 2)));

    assert_eq!(s.key(&tree, &layout, Key::Home), KeyResult::Handled);
    assert_eq!(s.focused, item(LIST, 0));
    assert_eq!(s.key(&tree, &layout, Key::Up), KeyResult::Handled);
    assert_eq!(s.focused, item(LIST, 0), "the first row is the other end");
    assert_eq!(s.key(&tree, &layout, Key::End), KeyResult::Handled);
    assert_eq!(s.focused, item(LIST, 2));
    assert_eq!(s.key(&tree, &layout, Key::Space), KeyResult::Action(Action::new(40, 2)));

    assert_eq!(s.key(&tree, &layout, Key::Right), KeyResult::Ignored);
}

#[test]
fn space_and_enter_emit_the_focused_widgets_action() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    assert_eq!(s.key(&tree, &layout, Key::Space), KeyResult::Ignored, "nothing is focused");
    s.key(&tree, &layout, Key::Tab);
    assert_eq!(s.key(&tree, &layout, Key::Space), KeyResult::Action(Action::new(10, 0)));
    assert_eq!(s.key(&tree, &layout, Key::Enter), KeyResult::Action(Action::new(10, 0)));
    s.key(&tree, &layout, Key::BackTab);
    assert_eq!(s.key(&tree, &layout, Key::Enter), KeyResult::Action(Action::new(50, 0)));
    assert_eq!(s.key(&tree, &layout, Key::Left), KeyResult::Ignored, "a button has no arrows");
}

#[test]
fn a_press_and_release_on_the_same_widget_emits_its_action() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    s.pointer_down(&tree, &layout, (50.0, 15.0));
    assert_eq!(s.pressed, on(A));
    assert_eq!(s.pointer_up(&tree, &layout, (60.0, 20.0)), Some(Action::new(10, 0)));
    assert_eq!(s.pressed, None);

    // a segment is its group's widget and its own item
    s.pointer_down(&tree, &layout, (50.0, 75.0));
    assert_eq!(s.pressed, item(SEG, 1));
    assert_eq!(s.pointer_up(&tree, &layout, (50.0, 75.0)), Some(Action::new(30, 1)));
}

#[test]
fn a_press_that_slides_off_before_release_emits_nothing() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    s.pointer_down(&tree, &layout, (50.0, 15.0));
    assert_eq!(s.pointer_up(&tree, &layout, (50.0, 165.0)), None, "released on another widget");
    assert_eq!(s.pressed, None);

    s.pointer_down(&tree, &layout, (50.0, 15.0));
    assert_eq!(s.pointer_up(&tree, &layout, (300.0, 300.0)), None, "released on nothing");

    // from one segment to its neighbour is off the item that was pressed
    s.pointer_down(&tree, &layout, (10.0, 75.0));
    assert_eq!(s.pointer_up(&tree, &layout, (50.0, 75.0)), None);

    // and a release with no press before it is not a click
    assert_eq!(s.pointer_up(&tree, &layout, (50.0, 15.0)), None);
}

#[test]
fn a_disabled_widget_neither_hovers_nor_presses() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    assert!(!s.pointer_move(&tree, &layout, (50.0, 45.0)));
    assert_eq!(s.hovered, None);
    s.pointer_down(&tree, &layout, (50.0, 45.0));
    assert_eq!(s.pointer_up(&tree, &layout, (50.0, 45.0)), None);
    // the disabled list row
    assert_eq!(hit_test(&tree, &layout, &s.scroll, s.offset, (50.0, 120.0)), None);
}

#[test]
fn the_focus_is_shown_after_a_key_and_hidden_after_a_press() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    assert!(!s.focus_visible);
    s.key(&tree, &layout, Key::Tab);
    assert!(s.focus_visible, "Tab shows it");

    s.pointer_down(&tree, &layout, (50.0, 165.0));
    assert!(!s.focus_visible, "a press hides it");
    assert_eq!(s.focused, on(C), "and takes the focus to what was pressed");
    s.pointer_up(&tree, &layout, (50.0, 165.0));

    s.key(&tree, &layout, Key::BackTab);
    assert!(s.focus_visible);
    assert_eq!(s.focused, item(LIST, 0), "Tab carries on from the pressed widget");

    s.pointer_down(&tree, &layout, (300.0, 300.0));
    assert!(!s.focus_visible, "a press on nothing hides it as well");
    s.key(&tree, &layout, Key::Down);
    assert!(s.focus_visible, "an arrow shows it");
}

#[test]
fn every_change_to_the_picture_moves_the_revision_and_nothing_else_does() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    let r0 = s.revision();
    assert!(s.pointer_move(&tree, &layout, (50.0, 15.0)));
    let r1 = s.revision();
    assert!(r1 > r0, "a new hover");
    assert!(!s.pointer_move(&tree, &layout, (60.0, 20.0)));
    assert_eq!(s.revision(), r1, "moving inside the same widget is not a change");
    assert!(s.pointer_move(&tree, &layout, (300.0, 300.0)));
    assert!(s.revision() > r1, "leaving it is");
    let r2 = s.revision();
    s.set_offset((0.0, 0.0));
    assert_eq!(s.revision(), r2);
    s.set_offset((4.0, 0.0));
    assert!(s.revision() > r2);
}

fn column_of_ten() -> (Tree, Layout) {
    let mut tree = Tree::new();
    tree.build(|ui| {
        ui.scroll(0, |ui| {
            for k in 0..10 {
                ui.leaf(1, L::Px(100.0), L::Px(30.0)).widget(WidgetId(100 + k), Action::new(7, k));
            }
        });
    });
    let mut layout = Layout::new();
    layout.run(&tree, (0.0, 0.0), Constraints::loose(400.0, 100.0), &METRICS, &MEASURE);
    (tree, layout)
}

#[test]
fn focusing_a_widget_outside_the_viewport_scrolls_it_into_view() {
    let (tree, layout) = column_of_ten();
    let mut s = Interaction::default();
    for _ in 0..3 {
        s.key(&tree, &layout, Key::Tab);
    }
    assert_eq!(s.scroll[0], 0.0, "the third row ends at 90, inside a viewport of 100");
    s.key(&tree, &layout, Key::Tab);
    assert_eq!(s.focused, on(WidgetId(103)));
    assert_eq!(s.scroll[0], 20.0, "the fourth ends at 120: its bottom is brought to the edge");
    for _ in 0..6 {
        s.key(&tree, &layout, Key::Tab);
    }
    assert_eq!(s.scroll[0], 200.0, "the last row, at the most the content can scroll");
    s.key(&tree, &layout, Key::Tab);
    assert_eq!(s.focused, on(WidgetId(100)));
    assert_eq!(s.scroll[0], 0.0, "round to the first: its top is brought back");
}

#[test]
fn a_row_cut_by_the_viewport_is_not_hit_until_it_is_scrolled_whole_into_view() {
    let (tree, layout) = column_of_ten();
    let mut s = Interaction::default();
    let at = |s: &Interaction, p| hit_test(&tree, &layout, &s.scroll, s.offset, p).map(|h| h.action.arg);
    assert_eq!(at(&s, (10.0, 65.0)), Some(2));
    assert_eq!(at(&s, (10.0, 95.0)), None, "row 3 is at 90..120 and the viewport ends at 100");
    assert_eq!(at(&s, (10.0, 130.0)), None, "below the viewport there is content, and no hit");

    assert!(s.wheel(&layout, (10.0, 50.0), 20.0));
    assert_eq!(at(&s, (10.0, 95.0)), Some(3), "now at 70..100");
    assert_eq!(at(&s, (10.0, 5.0)), None, "and row 0 is the one cut, at -20..10");
    assert_eq!(at(&s, (10.0, 15.0)), Some(1));

    assert!(s.wheel(&layout, (10.0, 50.0), 1.0e6));
    assert_eq!(s.scroll[0], 200.0, "clamped to what there is");
    assert!(!s.wheel(&layout, (10.0, 50.0), 5.0), "at the end, a wheel moves nothing");
    assert!(!s.wheel(&layout, (10.0, 250.0), -5.0), "outside the region, nothing to scroll");
}

#[test]
fn what_is_painted_on_top_is_what_is_hit() {
    // three overlapping boxes in one z-stack, the later above the earlier
    let (tree, layout) = lay(|ui| {
        ui.zstack(Align::TopLeading, |ui| {
            ui.leaf(1, L::Px(200.0), L::Px(200.0)).widget(WidgetId(1), Action::new(1, 0));
            ui.leaf(1, L::Px(120.0), L::Px(120.0)).widget(WidgetId(2), Action::new(2, 0));
            ui.vstack(|ui| {
                ui.leaf(1, L::Px(60.0), L::Px(60.0)).widget(WidgetId(3), Action::new(3, 0));
            });
        });
    });
    let at = |p| hit_test(&tree, &layout, &[0.0; 4], (0.0, 0.0), p).map(|h| h.action.id);
    assert_eq!(at((30.0, 30.0)), Some(3), "all three are here; the last described wins");
    assert_eq!(at((90.0, 90.0)), Some(2));
    assert_eq!(at((150.0, 150.0)), Some(1));
    assert_eq!(at((250.0, 150.0)), None);
}

#[test]
fn a_child_that_overflows_a_clipping_parent_is_not_hit_outside_it() {
    let (tree, layout) = lay(|ui| {
        ui.vstack(|ui| {
            ui.vstack(|ui| {
                ui.leaf(1, L::Px(100.0), L::Px(200.0)).widget(WidgetId(1), Action::new(1, 0));
            })
            .height(L::Px(50.0))
            .clip();
            ui.leaf(0, L::Px(100.0), L::Px(200.0));
        });
    });
    let at = |p| hit_test(&tree, &layout, &[0.0; 4], (0.0, 0.0), p).map(|h| h.action.id);
    assert_eq!(at((10.0, 25.0)), Some(1));
    // the widget's own box runs to 200, the clip ends at 50
    assert_eq!(at((10.0, 75.0)), None);
}

#[test]
fn a_dragged_surface_is_hit_where_it_is_drawn() {
    let (tree, layout) = form();
    let mut s = Interaction::default();
    s.set_offset((500.0, 40.0));
    let at = |s: &Interaction, p| hit_test(&tree, &layout, &s.scroll, s.offset, p).map(|h| h.target);
    assert_eq!(at(&s, (550.0, 55.0)), on(A));
    assert_eq!(at(&s, (50.0, 15.0)), None, "where it was laid out, there is nothing now");
    s.pointer_down(&tree, &layout, (550.0, 55.0));
    assert_eq!(s.pointer_up(&tree, &layout, (551.0, 56.0)), Some(Action::new(10, 0)));
}

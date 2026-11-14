//! The settings panel's surface, without a frame drawn: that it puts
//! everything where the panel's own walk says, that the keyboard
//! reaches every control and does what a press does, and that a
//! control scrolled out of sight answers nothing.

use super::settings_view::{CHOOSE, SettingsRender, choice, chosen};
use crate::font_cache::FontCache;
use crate::settings::Settings;
use crate::ui::components::controls;
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::settings_modal::{self as sm, Control, Row, Slot};
use crate::ui::tree::panel;
use golia_ui_core::node::flag;
use golia_ui_core::{Action, GroupKind, Key, KeyResult, NO_ITEM, NodeId, NodeKind, Rect, Target, WidgetId};

const TOP_INSET: f64 = 30.0;

/// The panel in a `window`, laid out, scrolled as far as it goes when
/// `to_end`.
fn laid_out(font: &mut FontCache, window: (f64, f64), to_end: bool, s: Settings) -> SettingsRender {
    let (rect, scroll) = {
        let mut m = controls::font_measure(font);
        let rect = sm::panel_rect(window.0, window.1, &s, &mut m, TOP_INSET);
        (rect, if to_end { sm::max_scroll(rect, &s, &mut m) } else { 0.0 })
    };
    let sp = SettingsRender::new(rect, scroll, s, "/tmp/settings.toml");
    sp.surface.borrow_mut().lay_out(font, &sp, TOP_INSET);
    sp
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

fn same(got: Rect, want: marspot_term::layout::Rect) -> bool {
    near(got.x, want.x) && near(got.y, want.y_top) && near(got.w, want.w) && near(got.h, want.h)
}

fn centre(r: Rect) -> (f64, f64) {
    (r.x + r.w * 0.5, r.y + r.h * 0.5)
}

/// Every rectangle the walk gives a row, a card and a control is the
/// rectangle of the node the surface made for it, at both scales.
#[test]
fn the_surface_puts_rows_cards_and_controls_where_the_walk_does() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            let s = Settings::default();
            let sp = laid_out(&mut font, (1000.0 * scale, 1400.0 * scale), false, s.clone());
            let surface = sp.surface.borrow();
            let (tree, layout) = (surface.kept.surface.tree(), surface.kept.surface.layout());
            let rect_of = |i: usize| layout.rect(NodeId(i as u32));
            let nodes = tree.nodes();
            let mut rows = nodes.iter().enumerate().filter(|(_, n)| n.has(flag::WHOLE)).map(|(i, _)| rect_of(i));
            let mut cards = nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.kind == NodeKind::VStack && n.style.border.is_some())
                .map(|(i, _)| rect_of(i));
            // a toggle is its leaf, a segmented control the group round its segments
            let mut controls_at = nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.widget.is_some_and(|id| id != panel::CLOSE_ID && id != panel::HEADER_ID))
                .filter(|(_, n)| n.item == NO_ITEM)
                .map(|(i, _)| rect_of(i));
            let mut rules = nodes.iter().enumerate().filter(|(_, n)| n.kind == NodeKind::Rule).map(|(i, _)| rect_of(i));
            let (mut n_rows, mut n_cards, mut n_rules) = (0, 0, 0);
            sm::walk(sp.rect, 0.0, &s, &mut controls::font_measure(&mut font), |slot| match slot {
                Slot::Card { rect } => {
                    let got = cards.next().expect("a card node for every card");
                    assert!(same(got, rect), "scale {scale}: card {n_cards} is at {got:?}, the walk has {rect:?}");
                    n_cards += 1;
                }
                Slot::Row { row, band, control, .. } => {
                    let got = rows.next().expect("a row node for every row");
                    assert!(same(got, band), "scale {scale}: {row:?} is at {got:?}, the walk has {band:?}");
                    let got = controls_at.next().expect("a control node for every row");
                    assert!(same(got, control), "scale {scale}: {row:?}'s control is at {got:?}, the walk has {control:?}");
                    n_rows += 1;
                }
                Slot::Separator { card, y } => {
                    let want = crate::ui::components::panel_chrome::row_separator_rect(card, y);
                    let got = rules.next().expect("a rule for every separator");
                    assert!(same(got, want), "scale {scale}: separator {n_rules} is at {got:?}, the walk has {want:?}");
                    n_rules += 1;
                }
                Slot::Group { .. } | Slot::Footer { .. } => {}
            });
            assert_eq!((n_rows, n_cards), (sm::rows().count(), sm::SECTIONS.len()));
            assert!(rows.next().is_none() && cards.next().is_none() && controls_at.next().is_none());
            assert!(n_rules > 0 && rules.next().is_none());
            // the frame is the panel's rect, and its content is as tall as the walk's
            assert!(same(rect_of(0), sp.rect));
            let extent = layout.scroll_extent(super::settings_view::SCROLL as usize);
            let want = sm::content_h(&s, &mut controls::font_measure(&mut font));
            assert!(near(extent.content_h, want), "scale {scale}: content {} against {want}", extent.content_h);
        });
    }
}

/// Tab stops at the close button and then at every row's control, in
/// order, and comes round; on each, the keyboard emits what a press on
/// the same control emits.
#[test]
fn the_keyboard_reaches_every_control_and_does_what_a_press_does() {
    let mut font = FontCache::build().expect("font");
    let s = Settings::default();
    let sp = laid_out(&mut font, (1000.0, 1400.0), false, s.clone());
    let mut surface = sp.surface.borrow_mut();
    let surface = &mut surface.kept.surface;
    let press = |surface: &mut golia_ui_core::Surface<golia_ui_core::NoOut>, t: Target| {
        let at = surface
            .tree()
            .nodes()
            .iter()
            .position(|n| n.widget == Some(t.id) && n.item == t.item && n.has(flag::ACTS))
            .expect("the control has a node");
        let p = centre(surface.layout().rect(NodeId(at as u32)));
        surface.pointer_down(p);
        surface.pointer_up(p)
    };

    assert_eq!(surface.interaction.focused, None, "a panel opens with nothing focused");
    assert_eq!(surface.key(Key::Tab), KeyResult::Handled);
    assert_eq!(surface.interaction.focused.map(|t| t.id), Some(panel::CLOSE_ID));
    assert!(surface.interaction.focus_visible);
    assert_eq!(surface.key(Key::Space), KeyResult::Action(panel::CLOSE));

    for (index, spec) in sm::rows().enumerate() {
        assert_eq!(surface.key(Key::Tab), KeyResult::Handled);
        let at = surface.interaction.focused.expect("Tab focused something");
        assert_eq!(at.id, WidgetId(index as u32), "stop {index} is not {:?}", spec.row);
        match spec.row.control(&s) {
            Control::Toggle(_) => {
                let want = Action::new(CHOOSE, choice(index, 0));
                assert_eq!(surface.key(Key::Space), KeyResult::Action(want));
                assert_eq!(surface.key(Key::Enter), KeyResult::Action(want));
                assert_eq!(press(surface, at), Some(want), "{:?}: a press", spec.row);
                assert_eq!(chosen(want.arg), Some((spec.row, 0)));
            }
            Control::Segmented { options, chosen: current } => {
                assert_eq!(at.item as usize, current, "{:?}: Tab lands on the segment in force", spec.row);
                // to the first segment, then along all of them
                for _ in 0..options.len() {
                    surface.key(Key::Left);
                }
                assert_eq!(surface.interaction.focused, Some(Target { id: at.id, item: 0 }));
                for seg in 1..options.len() {
                    let want = Action::new(CHOOSE, choice(index, seg));
                    assert_eq!(surface.key(Key::Right), KeyResult::Action(want), "{:?} segment {seg}", spec.row);
                    assert_eq!(surface.key(Key::Enter), KeyResult::Action(want));
                    let here = Target { id: at.id, item: seg as u32 };
                    assert_eq!(press(surface, here), Some(want), "{:?} segment {seg}: a press", spec.row);
                    assert_eq!(chosen(want.arg), Some((spec.row, seg)));
                }
                assert_eq!(surface.key(Key::Right), KeyResult::Handled, "past the last segment nothing is chosen");
            }
        }
    }
    assert_eq!(surface.key(Key::Tab), KeyResult::Handled);
    assert_eq!(surface.interaction.focused.map(|t| t.id), Some(panel::CLOSE_ID), "Tab comes round");
    assert_eq!(surface.key(Key::BackTab), KeyResult::Handled);
    assert_eq!(
        surface.interaction.focused.map(|t| t.id),
        Some(WidgetId(sm::rows().count() as u32 - 1)),
        "and back round the other way"
    );
    // a press ends the showing of the focus
    let t = surface.interaction.focused.unwrap();
    press(surface, t);
    assert!(!surface.interaction.focus_visible);
}

/// With reclamation off its two rows answer neither the pointer nor
/// Tab, and a segmented control has exactly one stop.
#[test]
fn a_row_that_is_greyed_out_is_skipped_and_swallows_the_press() {
    let mut font = FontCache::build().expect("font");
    let s = Settings { reclaim_enabled: false, ..Default::default() };
    let sp = laid_out(&mut font, (1000.0, 1400.0), false, s.clone());
    let mut surface = sp.surface.borrow_mut();
    let surface = &mut surface.kept.surface;
    let mut stops = Vec::new();
    loop {
        surface.key(Key::Tab);
        let id = surface.interaction.focused.unwrap().id;
        if stops.first() == Some(&id) {
            break;
        }
        stops.push(id);
    }
    let dimmed: Vec<usize> =
        sm::rows().enumerate().filter(|(_, r)| r.row.disabled_by(&s)).map(|(i, _)| i).collect();
    assert_eq!(dimmed.len(), 2);
    assert_eq!(stops.len(), 1 + sm::rows().count() - dimmed.len(), "{stops:?}");
    for i in dimmed {
        assert!(!stops.contains(&WidgetId(i as u32)));
        let node = surface.tree().nodes().iter().position(|n| n.widget == Some(WidgetId(i as u32))).unwrap();
        let p = centre(surface.layout().rect(NodeId(node as u32)));
        assert_eq!(surface.hit(p), None);
    }
    let groups = surface.tree().nodes().iter().filter(|n| n.group == GroupKind::Segmented).count();
    assert_eq!(groups, sm::rows().filter(|r| matches!(r.row.control(&s), Control::Segmented { .. })).count());
}

/// A control scrolled up past the panel's top answers nothing where it
/// would have been, and the focus arriving on a row out of view brings
/// it back in.
#[test]
fn a_control_scrolled_out_of_view_is_not_hit_and_the_focus_scrolls_to_its_row() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let mut font = FontCache::build().expect("font");
            let s = Settings::default();
            let window = (1600.0 * scale, 700.0 * scale);
            let sp = laid_out(&mut font, window, true, s.clone());
            let max = sp.scroll;
            assert!(max > 0.0, "the window is short enough to scroll");
            let mut above = None;
            let mut shown = None;
            let content = crate::ui::components::panel_chrome::PanelChrome::new(sp.rect, 0.0).content;
            sm::walk(sp.rect, max, &s, &mut controls::font_measure(&mut font), |slot| {
                if let Slot::Row { row, band, control, .. } = slot {
                    let at = (control.x + control.w * 0.5, control.y_top + control.h * 0.5);
                    if control.y_top + control.h < sp.rect.y_top && above.is_none() {
                        above = Some(at);
                    }
                    let whole = band.y_top >= content.y_top && band.y_top + band.h <= content.y_top + content.h;
                    if whole && matches!(row.control(&s), Control::Toggle(_)) {
                        shown = Some(at);
                    }
                }
            });
            let mut surface = sp.surface.borrow_mut();
            let surface = &mut surface.kept.surface;
            let above = above.expect("scrolling to the end puts a control above the panel");
            assert_eq!(surface.hit(above), None, "scale {scale}");
            let shown = shown.expect("a toggle is in view at the end");
            assert!(surface.hit(shown).is_some(), "scale {scale}: a control in view is not hit");

            // Tab to the first row: it is above the view, and comes into it
            surface.key(Key::Tab);
            surface.key(Key::Tab);
            assert_eq!(surface.interaction.focused.map(|t| t.id), Some(WidgetId(0)));
            let now = surface.interaction.scroll[super::settings_view::SCROLL as usize];
            assert!(now < max, "scale {scale}: the focused row was left out of view at {now}");
        });
    }
}

/// A control scrolled up under the header is not what a press there
/// reaches: the header is.
#[test]
fn a_control_scrolled_under_the_header_gives_way_to_the_header() {
    let mut font = FontCache::build().expect("font");
    let s = Settings::default();
    let window = (1600.0, 700.0);
    let at_rest = laid_out(&mut font, window, false, s.clone());
    let chrome = crate::ui::components::panel_chrome::PanelChrome::new(at_rest.rect, 0.0);
    let mut first = None;
    sm::walk(at_rest.rect, 0.0, &s, &mut controls::font_measure(&mut font), |slot| {
        if let Slot::Row { control, .. } = slot
            && first.is_none()
        {
            first = Some(control);
        }
    });
    let first = first.expect("a row");
    let p = (first.x + first.w * 0.5, first.y_top + first.h * 0.5);
    let on = at_rest.surface.borrow().kept.surface.hit(p).map(|h| h.action.id);
    assert_eq!(on, Some(CHOOSE), "at rest the control is under that point");

    // push it up to the middle of the header
    let header_mid = chrome.header.y_top + chrome.header.h * 0.5;
    let scroll = p.1 - header_mid;
    let sp = SettingsRender::new(at_rest.rect, scroll, s, "/tmp/settings.toml");
    sp.surface.borrow_mut().lay_out(&mut font, &sp, TOP_INSET);
    let hit = sp.surface.borrow().kept.surface.hit((p.0, header_mid)).map(|h| h.action);
    assert_eq!(hit, Some(panel::DRAG));
}

#[test]
fn a_choice_names_its_row_and_segment_and_nothing_else_does() {
    for (index, spec) in sm::rows().enumerate() {
        for seg in 0..5 {
            assert_eq!(chosen(choice(index, seg)), Some((spec.row, seg)));
        }
    }
    assert_eq!(chosen(choice(sm::rows().count(), 0)), None);
    assert_eq!(chosen(choice(0, 0)).map(|(row, _)| row), Some(Row::ReclaimEnabled));
}

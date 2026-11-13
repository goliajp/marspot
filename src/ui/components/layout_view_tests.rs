//! The layout panel's surface, without a frame drawn: its parts are
//! where the panel's geometry puts them, the keyboard reaches every
//! button, and a label is cut the same whichever way it is asked for.

use super::layout_view::{self, LayoutModalRender};
use crate::font_cache::FontCache;
use crate::ui::components::controls::{self, StepperPart};
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::layout_modal::{self as lm, Axis, Hover, LayoutModal};
use crate::ui::tree::panel;
use golia_ui_core::node::flag;
use golia_ui_core::{Key, KeyResult, NodeId, Rect};

fn laid_out(font: &mut FontCache, window: (f64, f64), cols: usize, rows: usize) -> (LayoutModalRender, LayoutModal) {
    let slot_titles: Vec<String> = (0..cols * rows).map(|n| format!("pane-{n}")).collect();
    let state = LayoutModalRender { cols, rows, slot_titles, ..Default::default() };
    state.surface.borrow_mut().lay_out(font, &state, window, 0.0);
    let modal = {
        let mut m = controls::font_measure(font);
        let label_w = lm::widest_label(&state.slot_titles, &mut m);
        LayoutModal::layout(window.0, window.1, 0.0, cols, rows, label_w, &mut m)
    };
    (state, modal)
}

fn same(got: Rect, want: marspot_term::layout::Rect) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
    near(got.x, want.x) && near(got.y, want.y_top) && near(got.w, want.w) && near(got.h, want.h)
}

/// The rectangle of the node that asks for `on`.
fn part(state: &LayoutModalRender, on: Hover) -> Rect {
    let s = state.surface.borrow();
    let (tree, layout) = (s.kept.surface.tree(), s.kept.surface.layout());
    let at = tree
        .nodes()
        .iter()
        .position(|n| n.has(flag::ACTS) && layout_view::asked(n.action) == on)
        .unwrap_or_else(|| panic!("nothing in the surface asks for {on:?}"));
    layout.rect(NodeId(at as u32))
}

#[test]
fn the_surface_puts_every_part_where_the_geometry_does() {
    for scale in [1.0, 2.0] {
        // roomy, and too short for the cards at their full height
        for (window, cols, rows) in [((1600.0, 1200.0), 4, 1), ((1600.0, 1200.0), 9, 3), ((1400.0, 420.0), 3, 4)] {
            at_scale(scale, || {
                let mut font = FontCache::build().expect("font");
                let window = (window.0 * scale, window.1 * scale);
                let (state, modal) = laid_out(&mut font, window, cols, rows);
                let what = format!("scale {scale}, {cols} by {rows} in {window:?}");
                assert!(same(part(&state, Hover::Close), modal.chrome.close), "{what}: close");
                assert!(same(part(&state, Hover::Apply), modal.apply_btn), "{what}: apply");
                for row in &modal.rows {
                    for (p, want) in [(StepperPart::Minus, row.stepper.minus), (StepperPart::Plus, row.stepper.plus)] {
                        // a button at its limit swallows the press and asks for nothing
                        let value = if row.axis == Axis::Columns { cols } else { rows };
                        if lm::step(value, p).is_some() {
                            assert!(same(part(&state, Hover::Step(row.axis, p)), want), "{what}: {:?} {p:?}", row.axis);
                        }
                    }
                }
                for (slot, want) in modal.cards.iter().enumerate() {
                    let got = part(&state, Hover::Card(slot));
                    assert!(same(got, *want), "{what}: card {slot} at {got:?}, the geometry has {want:?}");
                }
            });
        }
    }
}

#[test]
fn the_keyboard_reaches_every_button_and_asks_what_a_press_asks() {
    let mut font = FontCache::build().expect("font");
    // columns at their least, so the first stepper's minus is not a stop
    let (state, _) = laid_out(&mut font, (1600.0, 1200.0), 1, 3);
    let mut surface = state.surface.borrow_mut();
    let s = &mut surface.kept.surface;
    let want = [
        Hover::Close,
        Hover::Step(Axis::Columns, StepperPart::Plus),
        Hover::Step(Axis::Rows, StepperPart::Minus),
        Hover::Step(Axis::Rows, StepperPart::Plus),
        Hover::Apply,
    ];
    for on in want {
        assert_eq!(s.key(Key::Tab), KeyResult::Handled);
        let KeyResult::Action(by_key) = s.key(Key::Enter) else { panic!("Enter on {on:?} asked for nothing") };
        assert_eq!(layout_view::asked(by_key), on);
        assert_eq!(s.key(Key::Space), KeyResult::Action(by_key));
        let at = s.interaction.focused.expect("Tab focused something");
        let node = s.tree().nodes().iter().position(|n| n.widget == Some(at.id) && n.has(flag::ACTS)).unwrap();
        let r = s.layout().rect(NodeId(node as u32));
        let p = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        s.pointer_down(p);
        assert_eq!(s.pointer_up(p), Some(by_key), "{on:?}: a press asks for something else");
    }
    assert_eq!(s.key(Key::Tab), KeyResult::Handled);
    assert_eq!(s.interaction.focused.map(|t| t.id), Some(panel::CLOSE_ID), "Tab comes round");
}

#[test]
fn a_label_cut_into_a_buffer_is_the_label_cut_into_a_string() {
    // six pixels a character
    let mut m = |s: &str, _: f64, _: u16| s.chars().count() as f64 * 6.0;
    let mut out = String::from("left over from the last card");
    for s in ["", "a", "marspot", "lab38-golialab-experiments", "日本語のプロジェクト名", "a…b"] {
        for max_w in [0.0, 5.0, 6.0, 13.0, 40.0, 61.0, 1000.0] {
            lm::fit_label_into(s, max_w, &mut m, &mut out);
            assert_eq!(out, lm::fit_label(s, max_w, &mut m), "{s:?} in {max_w}");
            assert!(out == s || out.ends_with('…'));
            assert!(out == s || out.chars().count() as f64 * 6.0 <= max_w.max(6.0), "{out:?} does not fit {max_w}");
        }
    }
    lm::fit_label_into("marspot", 30.0, &mut m, &mut out);
    assert_eq!(out, "mars…", "four characters and the ellipsis are five times six");
}

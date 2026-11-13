//! A click lands on what is drawn there, at display scale 1 and at 2.
//!
//! What is drawn and what is clickable are worked out in different
//! places — a painter and a hit function — and a size one of them
//! scales and the other does not moves them apart only on the display
//! nobody is looking at.  So each thing the pointer can be on is drawn
//! twice, at rest and under the pointer; the rectangle that lights up
//! is the painted control, and its centre is given to the hit function
//! a click goes through, which has to answer with that control.

use super::chrome_snapshot_tests as chrome;
use super::density_kit::{capture, Capture, Kind};
use super::panel_snapshot_tests as panel;
use crate::render_metal::{MetalRenderer, WindowRender};
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::controls::StepperPart;
use crate::ui::components::{cc_usage_modal, layout_modal, process_panel};
use crate::ui::components::{ContextMenu, ContextMenuHit, MenuItem};

const SCALES: [f64; 2] = [1.0, 2.0];

/// The centre of what `lit` draws that `rest` does not: the last such
/// rectangle, which is the one on top.
fn lit_centre(rest: &Capture, lit: &Capture) -> Option<(f64, f64)> {
    let drawn_at_rest = |e: &super::density_kit::Elem| {
        rest.elems.iter().any(|r| r.layer == e.layer && r.kind == e.kind && r.edges == e.edges && r.tint == e.tint)
    };
    lit.elems
        .iter()
        .rfind(|e| matches!(e.kind, Kind::Fill | Kind::Rounded) && !drawn_at_rest(e))
        .map(|e| ((e.edges[0] + e.edges[2]) * 0.5, (e.edges[1] + e.edges[3]) * 0.5))
}

fn renderer() -> Option<(MetalRenderer, WindowRender)> {
    match MetalRenderer::new_headless() {
        Ok(r) => Some((r, WindowRender::new())),
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            None
        }
    }
}

#[test]
fn the_windows_controls_are_hit_where_they_are_drawn() {
    for scale in SCALES {
        let Some((mut r, mut wr)) = renderer() else { return };
        let mut frame = |hover| capture(scale, || chrome::window_on(&mut r, &mut wr, hover)).expect("a frame");
        let rest = frame(None);
        let layout = rest.layout.as_ref().expect("the window's layout");
        // every toolbar button the window shows, the add button, a row
        // not in use and its close mark, and the close mark of the row
        // in use
        let on = [
            ChromeHover::Toolbar(0), ChromeHover::Toolbar(1), ChromeHover::Toolbar(2), ChromeHover::Toolbar(3),
            ChromeHover::Toolbar(4), ChromeHover::Add, ChromeHover::Row(0), ChromeHover::Row(3),
            ChromeHover::Close(0), ChromeHover::Close(1), ChromeHover::Close(3),
        ];
        for hover in on {
            let (x, y) = lit_centre(&rest, &frame(Some(hover)))
                .unwrap_or_else(|| panic!("scale {scale}: nothing is drawn for the pointer on {hover:?}"));
            assert_eq!(
                super::density_kit::at_scale(scale, || ChromeHover::at(layout, 4, x, y)), Some(hover),
                "scale {scale}: {hover:?} is drawn around ({x:.1}, {y:.1}) and a click there lands elsewhere",
            );
        }
        // a pane's name, in its title strip
        for (i, (name, strip)) in rest.regions.iter().filter(|(name, _)| name.starts_with("the title strip")).enumerate() {
            let inside = |e: &&super::density_kit::Elem| {
                let (cx, cy) = ((e.edges[0] + e.edges[2]) * 0.5, (e.edges[1] + e.edges[3]) * 0.5);
                e.kind == Kind::Glyph && e.layer == "glyphs" && cx >= strip[0] && cx < strip[2] && cy >= strip[1] && cy < strip[3]
            };
            let glyph = rest.elems.iter().find(inside).unwrap_or_else(|| panic!("scale {scale}: no name is drawn in {name}"));
            let (x, y) = ((glyph.edges[0] + glyph.edges[2]) * 0.5, (glyph.edges[1] + glyph.edges[3]) * 0.5);
            assert_eq!(layout.hit_test_cell_title(x, y), Some(i), "scale {scale}: the name in {name} is not on its strip");
        }
    }
}

#[test]
fn the_settings_panels_controls_are_hit_where_they_are_drawn() {
    use crate::ui::components::settings_view::SettingsRender;
    use golia_ui_core::node::flag;
    use golia_ui_core::{NodeId, Target};
    for scale in SCALES {
        let Some((mut r, mut wr)) = renderer() else { return };
        fn frame(
            scale: f64,
            r: &mut MetalRenderer,
            wr: &mut WindowRender,
            pointer: Option<(f64, f64)>,
        ) -> (Capture, SettingsRender) {
            let mut placed = None;
            let frame = capture(scale, || {
                panel::snapshot_on(r, wr, "settings", 1000, 1360, |r, layout| {
                    placed = Some(panel::open_settings(r, layout, pointer));
                });
            });
            (frame.expect("a frame"), placed.expect("the panel was placed"))
        }
        let (rest, sp) = frame(scale, &mut r, &mut wr, None);
        // every control the panel has, and where its surface put it
        let controls: Vec<(Target, golia_ui_core::Rect)> = {
            let s = sp.surface.borrow();
            let (tree, layout) = (s.kept.surface.tree(), s.kept.surface.layout());
            tree.nodes()
                .iter()
                .enumerate()
                .filter(|(_, n)| n.leaf_tag != 0 && !n.has(flag::DISABLED))
                .filter_map(|(i, n)| Some((Target { id: n.widget?, item: n.item }, layout.rect(NodeId(i as u32)))))
                .collect()
        };
        assert!(controls.len() >= 20, "scale {scale}: the surface holds {} controls", controls.len());
        let mut checked = 0;
        for (control, at) in controls {
            let pointer = (at.x + at.w * 0.5, at.y + at.h * 0.5);
            let (lit, sp) = frame(scale, &mut r, &mut wr, Some(pointer));
            // a control with nothing to show for the pointer
            let Some((x, y)) = lit_centre(&rest, &lit) else { continue };
            let got = sp.surface.borrow().kept.surface.hit((x, y)).map(|h| h.target);
            assert_eq!(
                got,
                Some(control),
                "scale {scale}: {control:?} is drawn around ({x:.1}, {y:.1}) and a click there lands elsewhere"
            );
            checked += 1;
        }
        assert!(checked >= 10, "scale {scale}: only {checked} controls lit up; the check is not looking at the panel");
    }
}

#[test]
fn the_process_monitors_rows_and_buttons_are_hit_where_they_are_drawn() {
    use process_panel::Hover;
    for scale in SCALES {
        let Some((mut r, mut wr)) = renderer() else { return };
        let mut frame = |hover| {
            let mut placed = None;
            let frame = capture(scale, || {
                panel::snapshot_on(&mut r, &mut wr, "process", 1600, 1000, |r, layout| {
                    placed = Some(panel::open_process(r, layout, hover));
                });
            });
            (frame.expect("a frame"), placed.expect("the panel was placed"))
        };
        let (rest, _) = frame(Hover::None);
        let (cell_w, cell_h) = rest.cell;
        for hover in [Hover::Close, Hover::Pane(1), Hover::Pane(3), Hover::Process(0), Hover::Process(9), Hover::Kill(2), Hover::Kill(9)] {
            let (lit, (rect, n_panes, n_rows)) = frame(hover);
            let (x, y) = lit_centre(&rest, &lit)
                .unwrap_or_else(|| panic!("scale {scale}: nothing is drawn for the pointer on {hover:?}"));
            let got = super::density_kit::at_scale(scale, || {
                process_panel::layout(rect, 0.0, n_panes, n_rows, 0.0, cell_w, cell_h).hover_at(x, y)
            });
            assert_eq!(got, hover, "scale {scale}: {hover:?} is drawn around ({x:.1}, {y:.1}) and a click there lands elsewhere");
        }
    }
}

#[test]
fn the_layout_panels_controls_are_hit_where_they_are_drawn() {
    use layout_modal::{Axis, Hover};
    for scale in SCALES {
        let Some((mut r, mut wr)) = renderer() else { return };
        let mut frame = |hover| {
            let mut placed = None;
            let frame = capture(scale, || {
                panel::snapshot_on(&mut r, &mut wr, "layout", 1000, 760, |r, layout| {
                    placed = Some(panel::open_layout(r, layout, hover));
                });
            });
            (frame.expect("a frame"), placed.expect("the panel was placed"))
        };
        let (rest, _) = frame(Hover::None);
        let on = [
            Hover::Close, Hover::Apply, Hover::Step(Axis::Columns, StepperPart::Plus),
            Hover::Step(Axis::Columns, StepperPart::Minus), Hover::Step(Axis::Rows, StepperPart::Plus),
            Hover::Card(0), Hover::Card(3),
        ];
        let mut checked = 0;
        for hover in on {
            // a button that cannot be pressed shows nothing for the pointer
            let (lit, modal) = frame(hover);
            let Some((x, y)) = lit_centre(&rest, &lit) else { continue };
            let got = modal.hover_at(x, y);
            assert_eq!(got, hover, "scale {scale}: {hover:?} is drawn around ({x:.1}, {y:.1}) and a click there lands elsewhere");
            checked += 1;
        }
        assert!(checked >= 4, "scale {scale}: only {checked} controls lit up");
    }
}

#[test]
fn a_panels_close_button_is_hit_where_it_is_drawn() {
    use cc_usage_modal::Hover;
    for scale in SCALES {
        let Some((mut r, mut wr)) = renderer() else { return };
        let mut frame = |hover| {
            let mut placed = None;
            let frame = capture(scale, || {
                panel::snapshot_on(&mut r, &mut wr, "accounts", 2800, 1300, |r, layout| {
                    placed = Some(panel::open_accounts(r, layout, hover));
                });
            });
            (frame.expect("a frame"), placed.expect("the panel was placed"))
        };
        let (rest, _) = frame(Hover::None);
        let (lit, rect) = frame(Hover::Close);
        let (x, y) = lit_centre(&rest, &lit).expect("the close button lights up");
        let got = super::density_kit::at_scale(scale, || cc_usage_modal::hover_at(rect, x, y));
        assert_eq!(got, Hover::Close, "scale {scale}: the close button is drawn around ({x:.1}, {y:.1})");
    }
}

#[test]
fn a_context_menus_rows_are_hit_where_they_are_drawn() {
    for scale in SCALES {
        let Some((mut r, mut wr)) = renderer() else { return };
        let mut frame = |hovered| {
            let mut state = None;
            let frame = capture(scale, || state = Some(chrome::menu_on(&mut r, &mut wr, hovered)));
            (frame.expect("a frame"), state.expect("the menu was opened"))
        };
        let (rest, _) = frame(None);
        let layout = rest.layout.clone().expect("the window's layout");
        // the rows that can be picked: the first is disabled, the
        // fourth a divider
        for row in [1, 2, 4, 5] {
            let (lit, state) = frame(Some(row));
            let (x, y) = lit_centre(&rest, &lit)
                .unwrap_or_else(|| panic!("scale {scale}: nothing is drawn for the pointer on row {row}"));
            let items: Vec<MenuItem> = state
                .items
                .iter()
                .map(|i| MenuItem {
                    label: i.label.clone(),
                    shortcut_hint: i.shortcut_hint.clone(),
                    enabled: i.enabled,
                    divider: i.divider,
                    action_tag: 0,
                })
                .collect();
            let got = super::density_kit::at_scale(scale, || {
                ContextMenu::layout(
                    layout.window_w, layout.window_h, state.scale, state.anchor_phys.0, state.anchor_phys.1,
                    state.top_inset, &items,
                )
                .hit_test(&items, x, y)
            });
            assert_eq!(got, ContextMenuHit::Item(row), "scale {scale}: row {row} is drawn around ({x:.1}, {y:.1})");
        }
    }
}

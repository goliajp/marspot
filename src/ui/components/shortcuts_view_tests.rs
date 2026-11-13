//! The shortcuts panel's surface, without a frame drawn: it shows the
//! rows the panel's walk shows, where the walk puts them, whole or cut
//! short, and its one control answers the keyboard.

use super::shortcuts_view::ShortcutsRender;
use crate::font_cache::FontCache;
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::shortcuts_panel::{self as sp, Slot};
use crate::ui::tree::{panel, preset};
use golia_ui_core::{Key, KeyResult, NodeId, NodeKind};

fn laid_out(font: &mut FontCache, window: (f64, f64)) -> (ShortcutsRender, (f64, f64)) {
    let cell = (font.cell_w as f32 as f64, font.cell_h as f32 as f64);
    let panel = ShortcutsRender::new(sp::panel_rect(window.0, window.1, cell, 0.0));
    panel.surface.borrow_mut().lay_out(font, &panel, 0.0);
    (panel, cell)
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn the_surface_shows_the_rows_and_cards_the_walk_shows_where_it_puts_them() {
    for scale in [1.0, 2.0] {
        // whole, and at heights that cut a row, a card and a group
        for h in [1200.0, 640.0, 470.0, 340.0, 260.0] {
            at_scale(scale, || {
                let mut font = FontCache::build().expect("font");
                let (panel, cell) = laid_out(&mut font, (1600.0 * scale, h * scale));
                let surface = panel.surface.borrow();
                let (tree, layout) = (surface.kept.surface.tree(), surface.kept.surface.layout());
                let at = |i: usize| layout.rect(NodeId(i as u32));
                let nodes = tree.nodes();
                let keys: Vec<_> =
                    nodes.iter().enumerate().filter(|(_, n)| n.leaf_tag == preset::KEYS).map(|(i, _)| at(i)).collect();
                let cards: Vec<_> = nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| n.kind == NodeKind::VStack && n.style.border.is_some())
                    .map(|(i, _)| at(i))
                    .collect();
                let rules = nodes.iter().filter(|n| n.kind == NodeKind::Rule).count();
                let (mut n_rows, mut n_cards, mut n_rules) = (0, 0, 0);
                sp::walk(panel.rect, cell, |slot| match slot {
                    Slot::Card { rect } => {
                        let got = cards[n_cards];
                        assert!(
                            near(got.x, rect.x) && near(got.y, rect.y_top) && near(got.w, rect.w) && near(got.h, rect.h),
                            "scale {scale}, {h}: card {n_cards} at {got:?}, the walk has {rect:?}"
                        );
                        n_cards += 1;
                    }
                    Slot::Row { caps, .. } => {
                        let (got, last) = (keys[n_rows], caps.last().expect("a row has keys"));
                        assert!(
                            near(got.x + got.w, last.rect.x + last.rect.w) && near(got.y, last.rect.y_top) && near(got.h, last.rect.h),
                            "scale {scale}, {h}: row {n_rows}'s keys at {got:?}, the walk ends them at {:?}",
                            last.rect
                        );
                        n_rows += 1;
                    }
                    Slot::Separator { .. } => n_rules += 1,
                    Slot::Group { .. } => {}
                });
                assert_eq!((keys.len(), cards.len(), rules), (n_rows, n_cards, n_rules), "scale {scale}, {h}");
                if h == 1200.0 {
                    assert_eq!(n_rows, crate::commands::reference().len(), "every chord is shown");
                } else {
                    assert!(n_rows < crate::commands::reference().len(), "{h} is not short enough to cut anything");
                }
            });
        }
    }
}

#[test]
fn the_close_button_is_the_panels_one_stop_and_enter_closes() {
    let mut font = FontCache::build().expect("font");
    let (panel, _) = laid_out(&mut font, (1600.0, 1200.0));
    let mut surface = panel.surface.borrow_mut();
    let s = &mut surface.kept.surface;
    assert_eq!(s.key(Key::Enter), KeyResult::Ignored, "nothing is focused when a panel opens");
    for _ in 0..2 {
        assert_eq!(s.key(Key::Tab), KeyResult::Handled);
        assert_eq!(s.interaction.focused.map(|t| t.id), Some(panel::CLOSE_ID));
    }
    assert_eq!(s.key(Key::Enter), KeyResult::Action(panel::CLOSE));
    assert_eq!(s.key(Key::Space), KeyResult::Action(panel::CLOSE));
}

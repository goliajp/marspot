//! The process monitor's surface, without a frame drawn: the pointer
//! finds the rows, the kill buttons and the close button where the
//! panel's layout has them and nothing where it has nothing, and the
//! keyboard moves through the panes and never reaches a kill button.

use super::process_view::{self, ProcessPanelRender};
use crate::font_cache::FontCache;
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::process_panel::{self, Hover, ProcessPanelPaneRow, ProcessPanelRow};
use crate::ui::core::ViewPainter;
use crate::ui::tree::panel as tree_panel;
use golia_ui_core::{Key, KeyResult};
use marspot_term::layout::Rect;

const WINDOW: (f64, f64) = (1600.0, 1000.0);

/// The panel with `n_panes` panes and `n_rows` processes, drawn once
/// into buffers nobody looks at so that its surface is laid out.
fn laid_out(n_panes: usize, n_rows: usize, scroll_rows: f64) -> Option<ProcessPanelRender> {
    use crate::render_metal::{new_atlas, system_default_device};
    let Ok(device) = system_default_device() else {
        eprintln!("SKIPPED: no Metal device");
        return None;
    };
    let scale = crate::ui::chrome_scale();
    let mut font = FontCache::build().expect("font");
    let mut atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let (atlas_w, atlas_h) = atlas.dims();
    let (cell_w, cell_h) = (font.cell_w as f32 as f64, font.cell_h as f32 as f64);
    let window = (WINDOW.0 * scale, WINDOW.1 * scale);
    let frame = process_panel::panel_rect(window.0, window.1, cell_h, 0.0, (0.0, 0.0));
    let row_h = crate::ui::components::table::row_height(cell_h);
    let panel = ProcessPanelRender {
        layout: process_panel::layout(frame, 0.0, n_panes, n_rows, scroll_rows * row_h, cell_w, cell_h),
        pane_rows: (0..n_panes)
            .map(|i| ProcessPanelPaneRow {
                name: format!("pane-{i}"),
                sid: i as u64,
                n_pids: 2,
                cpu_pct: 1.0,
                rss_kb: 4096,
                busy: String::new(),
            })
            .collect(),
        selected_pane: 1,
        detail: String::new(),
        rows: (0..n_rows)
            .map(|i| ProcessPanelRow { depth: 0, pid: 100 + i as i32, comm: "zsh".to_string(), cpu_pct: 0.0, rss_kb: 2048 })
            .collect(),
        pointer: None,
        surface: Default::default(),
    };
    let (mut cells, mut glyphs, mut ui_rects) = (Vec::new(), Vec::new(), Vec::new());
    let mut p = ViewPainter {
        cell_w: font.cell_w as f32,
        cell_h: font.cell_h as f32,
        ascent: font.ascent as f32,
        atlas_w: atlas_w as f32,
        atlas_h: atlas_h as f32,
        window_w: window.0,
        window_h: window.1,
        font: &mut font,
        atlas: &mut atlas,
        cells: &mut cells,
        glyphs: &mut glyphs,
        ui_rects: &mut ui_rects,
    };
    panel.surface.borrow_mut().frame(&mut p, &panel);
    Some(panel)
}

fn on(panel: &ProcessPanelRender, x: f64, y: f64) -> Hover {
    panel.surface.borrow().kept.surface.hit((x, y)).map_or(Hover::None, |h| process_view::asked(h.action))
}

fn centre(r: Rect) -> (f64, f64) {
    (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
}

#[test]
fn the_pointer_finds_rows_buttons_and_the_close_button_where_the_layout_has_them() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let Some(panel) = laid_out(4, 10, 0.0) else { return };
            let l = &panel.layout;
            let (cx, cy) = centre(l.chrome.close);
            assert_eq!(on(&panel, cx, cy), Hover::Close, "scale {scale}");
            assert_eq!(on(&panel, l.chrome.close.x + 0.5, l.chrome.close.y_top + 0.5), Hover::Close);
            assert_eq!(on(&panel, l.chrome.close.x - 1.0, cy), Hover::None, "beside it is the header");
            assert_eq!(on(&panel, cx, l.chrome.close.y_top + l.chrome.close.h + 1.0), Hover::None);

            assert_eq!(l.kills.len(), 10, "every laid-out row has a button");
            for &(i, k) in &l.kills {
                // every corner of the rect is the button; just outside it is the row
                for (x, y) in [
                    (k.x + 0.01, k.y_top + 0.01),
                    (k.x + k.w - 0.01, k.y_top + 0.01),
                    (k.x + 0.01, k.y_top + k.h - 0.01),
                    (k.x + k.w - 0.01, k.y_top + k.h - 0.01),
                ] {
                    assert_eq!(on(&panel, x, y), Hover::Kill(i), "scale {scale}: a corner of kill button {i}");
                }
                assert_eq!(on(&panel, k.x - 0.5, k.y_top + 0.01), Hover::Process(i));
                assert_eq!(on(&panel, k.x + 0.01, k.y_top - 0.5), Hover::Process(i));
                assert_eq!(on(&panel, k.x + 0.01, k.y_top + k.h + 0.01), Hover::Process(i));
            }
            for &(i, r) in &l.panes.rows {
                assert_eq!(on(&panel, r.x + 30.0, r.y_top + r.h * 0.5), Hover::Pane(i), "scale {scale}: pane row {i}");
            }
            let (_, r) = l.processes.rows[3];
            assert_eq!(on(&panel, r.x + 30.0, r.y_top + 1.0), Hover::Process(3));

            // the headings, the tables' headers and the gap between the cards are nothing
            let (hx, hy) = centre(l.panes.header);
            assert_eq!(on(&panel, hx, hy), Hover::None);
            assert_eq!(on(&panel, l.panes_heading.x + 4.0, l.panes_heading.top + 4.0), Hover::None);
            let between = l.panes.card.x + l.panes.card.w + 2.0;
            assert_eq!(on(&panel, between, r.y_top + 1.0), Hover::None);
            assert_eq!(on(&panel, l.chrome.frame.x - 1.0, r.y_top), Hover::None, "outside the frame");
        });
    }
}

#[test]
fn rows_scrolled_out_of_the_card_are_not_there_to_be_pressed() {
    let Some(panel) = laid_out(4, 50, 0.0) else { return };
    let g = &panel.layout.processes;
    let bottom = g.card.y_top + g.card.h;
    assert_eq!(on(&panel, g.card.x + 20.0, bottom + 5.0), Hover::None, "below the card");
    let last = g.rows.last().expect("rows").0;
    assert!(last < 49, "fifty rows fit; this needs some not to");

    let Some(panel) = laid_out(4, 50, 1000.0) else { return };
    let g = &panel.layout.processes;
    let (first, r) = g.rows[0];
    assert!(first > 0, "scrolled to the end the first rows are gone");
    assert_eq!(on(&panel, r.x + 30.0, r.y_top + 1.0), Hover::Process(first));
    assert_eq!(g.rows.last().unwrap().0, 49);
}

#[test]
fn the_arrows_move_through_the_panes_and_tab_never_stops_on_a_kill_button() {
    let Some(panel) = laid_out(4, 10, 0.0) else { return };
    let mut surface = panel.surface.borrow_mut();
    let s = &mut surface.kept.surface;
    let asked = |r: KeyResult| match r {
        KeyResult::Action(a) => process_view::asked(a),
        _ => Hover::None,
    };
    let mut stops = Vec::new();
    for _ in 0..4 {
        assert_eq!(s.key(Key::Tab), KeyResult::Handled);
        stops.push(s.interaction.focused.expect("a stop").id);
    }
    assert_eq!(stops[0], tree_panel::CLOSE_ID);
    assert_eq!(stops[2], stops[0], "two stops: the close button and the list of panes");
    assert_eq!(stops[1], stops[3]);
    // on the list: it lands on the pane that is chosen, and moves from there
    assert_eq!(asked(s.key(Key::Enter)), Hover::Pane(1));
    assert_eq!(s.key(Key::Down), KeyResult::Handled);
    assert_eq!(asked(s.key(Key::Enter)), Hover::Pane(2));
    assert_eq!(s.key(Key::Home), KeyResult::Handled);
    assert_eq!(asked(s.key(Key::Space)), Hover::Pane(0));
    assert_eq!(s.key(Key::End), KeyResult::Handled);
    assert_eq!(asked(s.key(Key::Enter)), Hover::Pane(3));
}

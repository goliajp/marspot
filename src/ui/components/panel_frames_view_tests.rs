//! What a frame of each panel costs drawn through its surface: nothing
//! at all while nothing changes, a repaint when the pointer moves onto
//! a control, and all three stages, without an allocation, when what
//! the panel shows changes.  The settings panel's own is
//! `settings_surface_frame`.

use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::glyph_atlas::GlyphAtlas;
use crate::render_metal::{new_atlas, system_default_device};
use crate::ui::components::cc_usage_modal::{self, CcUsageRender};
use crate::ui::components::layout_view::LayoutModalRender;
use crate::ui::components::panel_snapshot_tests::accounts_data;
use crate::ui::components::process_panel::{self, ProcessPanelPaneRow, ProcessPanelRender, ProcessPanelRow};
use crate::ui::components::shortcuts_panel;
use crate::ui::components::shortcuts_view::ShortcutsRender;
use crate::ui::components::table;
use crate::ui::core::ViewPainter;
use crate::ui::tree::{preset, KeptSurface};
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};
use golia_ui_core::{Counters, NodeId};

const WINDOW: (f64, f64) = (1600.0, 1000.0);

/// A font, an atlas and the three instance arrays a frame is drawn
/// into.
struct Stage {
    font: FontCache,
    atlas: GlyphAtlas,
    cells: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    ui_rects: Vec<UiRectInstance>,
}

impl Stage {
    fn new() -> Option<Self> {
        let Ok(device) = system_default_device() else {
            eprintln!("SKIPPED: no Metal device");
            return None;
        };
        let font = FontCache::build().expect("font");
        let atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        Some(Self { font, atlas, cells: Vec::new(), glyphs: Vec::new(), ui_rects: Vec::new() })
    }

    fn cell(&self) -> (f64, f64) {
        (self.font.cell_w as f32 as f64, self.font.cell_h as f32 as f64)
    }

    /// One frame; how many instances of each kind it left.
    fn frame(&mut self, draw: &mut dyn FnMut(&mut ViewPainter<'_>)) -> (usize, usize, usize) {
        self.cells.clear();
        self.glyphs.clear();
        self.ui_rects.clear();
        let (atlas_w, atlas_h) = self.atlas.dims();
        let mut p = ViewPainter {
            cell_w: self.font.cell_w as f32,
            cell_h: self.font.cell_h as f32,
            ascent: self.font.ascent as f32,
            atlas_w: atlas_w as f32,
            atlas_h: atlas_h as f32,
            window_w: WINDOW.0,
            window_h: WINDOW.1,
            font: &mut self.font,
            atlas: &mut self.atlas,
            cells: &mut self.cells,
            glyphs: &mut self.glyphs,
            ui_rects: &mut self.ui_rects,
        };
        draw(&mut p);
        (self.cells.len(), self.glyphs.len(), self.ui_rects.len())
    }
}

/// The three kinds of frame of one panel.  `draw` paints it showing
/// one of two values of its data; `kept` lends its surface.
fn costs(
    name: &str,
    stage: &mut Stage,
    draw: &mut dyn FnMut(&mut ViewPainter<'_>, bool),
    kept: &dyn Fn(&mut dyn FnMut(&mut KeptSurface)),
) {
    let read = || {
        let mut c = Counters::default();
        kept(&mut |k| c = k.surface.counters());
        c
    };
    let mut frame = |other: bool| stage.frame(&mut |p| draw(p, other));
    // both values twice: every buffer has grown to what either needs
    for other in [false, true, false, true, false] {
        frame(other);
    }
    let warm = read();
    assert_eq!((warm.build, warm.layout, warm.paint), (5, 5, 5), "{name}: each new value is all three stages");

    let (idle_n, drawn) = counted(|| frame(false));
    assert_eq!(read(), warm, "{name}: a frame with nothing new ran a stage");
    assert_eq!(idle_n, 0, "{name}: a frame with nothing new allocated {idle_n} times");
    assert!(drawn.1 > 10 && drawn.2 > 2, "{name}: the panel has to have been painted");

    // the pointer onto the close button, off it and onto it again: the
    // first time, the kept instances grow by the button's hover fill
    let onto_close = || {
        kept(&mut |k| {
            let at = k.surface.tree().nodes().iter().position(|n| n.leaf_tag == preset::CLOSE).expect("a close button");
            let r = k.surface.layout().rect(NodeId(at as u32));
            assert!(k.surface.pointer_move((r.x + r.w * 0.5, r.y + r.h * 0.5)), "{name}: the pointer is on nothing");
        })
    };
    onto_close();
    frame(false);
    kept(&mut |k| {
        k.surface.pointer_leave();
    });
    frame(false);
    let warm = read();
    onto_close();
    let (hover_n, _) = counted(|| frame(false));
    let hovered = read();
    assert_eq!(
        (hovered.build, hovered.layout, hovered.paint),
        (warm.build, warm.layout, warm.paint + 1),
        "{name}: a hover is a repaint and nothing else"
    );
    assert_eq!(hover_n, 0, "{name}: a frame with the pointer moved allocated {hover_n} times");
    kept(&mut |k| {
        k.surface.pointer_leave();
    });
    frame(false);

    let before = read();
    let (data_n, _) = counted(|| frame(true));
    let changed = read();
    assert_eq!(
        (changed.build, changed.layout, changed.paint),
        (before.build + 1, before.layout + 1, before.paint + 1),
        "{name}: new data is all three stages"
    );
    assert_eq!(data_n, 0, "{name}: a frame with new data allocated {data_n} times");

    let median = |times: &mut Vec<u128>| {
        times.sort_unstable();
        times[times.len() / 2] as f64 / 1000.0
    };
    let mut timed = |other: &dyn Fn(usize) -> bool| -> Vec<u128> {
        (0..30)
            .map(|k| {
                let t = std::time::Instant::now();
                std::hint::black_box(frame(other(k)));
                t.elapsed().as_nanos()
            })
            .collect()
    };
    let mut idle = timed(&|_| true);
    let mut rebuilt = timed(&|k| k % 2 == 0);
    let mut nodes = 0;
    kept(&mut |k| nodes = k.surface.tree().len());
    eprintln!(
        "[frame-alloc] {name} through its surface, {nodes} nodes, {} flat rects, {} rounded rects, {} glyphs: \
         idle frame {idle_n} allocations, {:.1} us median of 30; hover frame {hover_n} allocations; \
         new-data frame {data_n} allocations, {:.1} us median of 30",
        drawn.0,
        drawn.2,
        drawn.1,
        median(&mut idle),
        median(&mut rebuilt),
    );
}

#[test]
fn shortcuts_surface_frame() {
    let Some(mut stage) = Stage::new() else { return };
    let cell = stage.cell();
    let whole = ShortcutsRender::new(shortcuts_panel::panel_rect(WINDOW.0, WINDOW.1, cell, 0.0));
    // nothing the panel shows is data: its other value is the window
    // made too short for all of its rows
    let short = ShortcutsRender {
        rect: shortcuts_panel::panel_rect(WINDOW.0, 420.0, cell, 0.0),
        pointer: None,
        surface: whole.surface.clone(),
    };
    costs(
        "shortcuts panel",
        &mut stage,
        &mut |p, other| {
            let sp = if other { &short } else { &whole };
            sp.surface.borrow_mut().frame(p, sp, 0.0)
        },
        &|f| f(&mut whole.surface.borrow_mut().kept),
    );
}

#[test]
fn layout_surface_frame() {
    let Some(mut stage) = Stage::new() else { return };
    let titles = ["smix", "devops", "insight-mono", "alt", "marspot", "a-name-no-card-can-hold-in-one-line", "七", "8", "9"];
    let slot_titles: Vec<String> = titles.map(String::from).to_vec();
    let one = LayoutModalRender { cols: 3, rows: 3, slot_titles: slot_titles.clone(), ..Default::default() };
    let mut renamed = slot_titles;
    renamed[1] = "renamed".to_string();
    let other = LayoutModalRender { slot_titles: renamed, ..one.clone() };
    costs(
        "layout panel",
        &mut stage,
        &mut |p, second| {
            let state = if second { &other } else { &one };
            state.surface.borrow_mut().frame(p, state, 0.0)
        },
        &|f| f(&mut one.surface.borrow_mut().kept),
    );
}

#[test]
fn accounts_surface_frame() {
    let Some(mut stage) = Stage::new() else { return };
    let cell = stage.cell();
    let (accounts, updated_label, clock) = accounts_data();
    let rows = accounts.iter().map(|a| a.windows.len()).max().unwrap_or(0);
    let rect = cc_usage_modal::panel_rect(accounts.len(), rows, WINDOW.0, WINDOW.1, cell.0, cell.1, 0.0);
    let mut moved = accounts.clone();
    moved[0].windows[0].util = Some(0.42);
    moved[3].reset_label.push_str(" +1");
    let surface = std::rc::Rc::default();
    let other = CcUsageRender {
        rect,
        updated_label: updated_label.clone(),
        accounts: moved,
        clock,
        pointer: None,
        surface: std::rc::Rc::clone(&surface),
    };
    let one = CcUsageRender { rect, updated_label, accounts, clock, pointer: None, surface };
    costs(
        "accounts panel",
        &mut stage,
        &mut |p, second| {
            let cc = if second { &other } else { &one };
            cc.surface.borrow_mut().frame(p, cc, 0.0)
        },
        &|f| f(&mut one.surface.borrow_mut().kept),
    );
}

#[test]
fn process_surface_frame() {
    let Some(mut stage) = Stage::new() else { return };
    let cell = stage.cell();
    let names = ["smix", "devops", "insight-mono", "alt", "日本語のペイン", "a-pane-with-a-name-longer-than-its-column"];
    let pane_rows: Vec<ProcessPanelPaneRow> = names
        .iter()
        .enumerate()
        .map(|(i, name)| ProcessPanelPaneRow {
            name: name.to_string(),
            sid: 479 + i as u64,
            n_pids: 3 + i as u32,
            cpu_pct: i as f32 * 7.3,
            rss_kb: 69_000 + i as u64 * 811_000,
            busy: String::new(),
        })
        .collect();
    let comms = ["zsh", "2.1.322", "smix-mcp", "sourcekit-lsp", "caffeinate", "rust-analyzer", "rust-analyzer-proc-macro-srv"];
    let rows: Vec<ProcessPanelRow> = (0..24)
        .map(|i| ProcessPanelRow {
            depth: (i % 4) as u8,
            pid: 1544 + i as i32 * 977,
            comm: comms[i % comms.len()].to_string(),
            cpu_pct: if i % 3 == 0 { 0.0 } else { i as f32 * 1.7 },
            rss_kb: 2_000 + i as u64 * 53_000,
        })
        .collect();
    let frame = process_panel::panel_rect(WINDOW.0, WINDOW.1, cell.1, 0.0, (0.0, 0.0));
    let layout = process_panel::layout(frame, 0.0, pane_rows.len(), rows.len(), table::row_height(cell.1) * 2.5, cell.0, cell.1);
    let mut sampled = rows.clone();
    sampled[1].cpu_pct = 31.4;
    sampled[2].rss_kb = 3_000_000;
    sampled[4].comm = "sleep".to_string();
    let one = ProcessPanelRender {
        layout,
        pane_rows,
        selected_pane: 1,
        detail: "devops · sid 480 · shell pid 1544".to_string(),
        rows,
        pointer: None,
        surface: Default::default(),
    };
    let other = ProcessPanelRender { rows: sampled, ..one.clone() };
    costs(
        "process monitor",
        &mut stage,
        &mut |p, second| {
            let panel = if second { &other } else { &one };
            panel.surface.borrow_mut().frame(p, panel)
        },
        &|f| f(&mut one.surface.borrow_mut().kept),
    );
}

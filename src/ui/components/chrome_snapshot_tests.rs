//! Offscreen pictures of the main window's own chrome, compared with
//! committed PNGs: the toolbar, the sidebar, the pane title strips and
//! seams, the search overlay and the context menu.
//!
//! Same machinery and the same way of recording and fetching as the
//! panel pictures (see `panel_snapshot_tests`); the tests are
//! `chrome_snapshot_{window,search,menu,close,terminal}` and the
//! baselines `bench/ui/snapshots/chrome_{window,search,menu,close,terminal}.png`.
//! `before/` beside them holds the first three scenes as they were
//! drawn before the chrome took its colours and sizes from the theme.
//! All but `close` are drawn again under the light and the
//! high-contrast theme, as `chrome_<scene>.{light,contrast}.png`.

use super::panel_snapshot_tests::terminal_text;
use crate::grid::{Cell, CellAttrs, Color, Grid};
use crate::render::{HighlightSpan, SelectionView};
use crate::layout::Layout;
use crate::render::{SearchHitView, SearchOverlayView, SessionView, SidebarEntry};
use crate::render_metal::{make_readback_texture, MetalRenderer, WindowRender};
use crate::session::SessionState;
use crate::snapshot_kit::{assert_snapshot_ssim, snapshot_mode_in, write_snapshot_png, UI_SNAPSHOT_DIR};
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::context_menu_paint::{ContextMenuRender, ContextMenuRow};

/// One pane of a scene.
struct PaneSpec {
    title: &'static str,
    badge: &'static str,
    state: SessionState,
    search: Option<SearchOverlayView>,
    /// What the pane's grid holds, at its size in cells.
    content: fn(u16, u16) -> Grid,
    selection: Option<SelectionView>,
    hits: &'static [HighlightSpan],
}

fn pane(title: &'static str, badge: &'static str, state: SessionState) -> PaneSpec {
    PaneSpec { title, badge, state, search: None, content: terminal_text, selection: None, hits: &[] }
}

/// A screen that uses every terminal colour: the sixteen as text and
/// as backgrounds, the SGR attributes, and a line each for a
/// selection, a search hit and a link to land on.
fn ansi_scene(cols: u16, rows: u16) -> Grid {
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    let mut g = Grid::new(cols, rows);
    let mut put = |row: u16, col: u16, text: &str, attrs: CellAttrs| {
        for (i, ch) in text.chars().enumerate() {
            g.set_cell(col + i as u16, row, Cell { ch, attrs });
        }
        col + text.chars().count() as u16
    };
    let plain = CellAttrs::default();
    put(0, 0, "$ colours", plain);
    for bright in [0u8, 8] {
        let row = 1 + bright as u16 / 8;
        let mut col = 0;
        for (i, name) in NAMES.iter().enumerate() {
            let fg = Color::indexed(bright + i as u8);
            col = put(row, col, name, CellAttrs { fg, ..plain }) + 1;
        }
    }
    for i in 0..16u8 {
        put(3, i as u16 * 3, "   ", CellAttrs { bg: Color::indexed(i), ..plain });
    }
    let mut col = put(4, 0, "bold", CellAttrs { bold: true, ..plain }) + 1;
    col = put(4, col, "dim", CellAttrs { dim: true, ..plain }) + 1;
    col = put(4, col, "underline", CellAttrs { underline: true, ..plain }) + 1;
    col = put(4, col, "reverse", CellAttrs { reverse: true, ..plain }) + 1;
    put(4, col, "red on yellow", CellAttrs { fg: Color::indexed(1), bg: Color::indexed(3), ..plain });
    put(5, 0, "select: the quick brown fox", plain);
    put(6, 0, "search: a needle in a haystack", plain);
    put(7, 0, "link: https://example.com/docs/themes", plain);
    put(8, 0, "$ ", plain);
    g.set_cursor(2, 8);
    g
}

/// `ansi_scene` with "quick brown" selected and "needle" a search hit.
fn ansi_pane(title: &'static str) -> PaneSpec {
    PaneSpec {
        content: ansi_scene,
        selection: Some(SelectionView { anchor: (12, 5), focus: (22, 5), blockwise: false }),
        hits: &[HighlightSpan { view_row: 6, col_start: 10, col_end_inclusive: 15 }],
        ..pane(title, "", SessionState::Active)
    }
}

/// Draw one frame of a `w × h` px window laid out as `cols × rows`
/// panes with the sidebar shown, `focused` the pane in use, and hold
/// it to `bench/ui/snapshots/<name>.png`.  `open` sets whatever else
/// the scene has on the renderer.
fn snapshot(
    name: &str,
    (w, h): (u32, u32),
    (cols, rows): (usize, usize),
    panes: &[PaneSpec],
    focused: usize,
    open: impl FnOnce(&mut MetalRenderer, &Layout),
) {
    let name = &crate::snapshot_kit::themed(name);
    let dir = std::path::Path::new(UI_SNAPSHOT_DIR);
    if snapshot_mode_in(dir).is_none() {
        return;
    }
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let target = make_readback_texture(renderer.device(), w, h).expect("a readback texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    // The app's own numbers at display scale 1.
    let scale = crate::ui::chrome_scale();
    let title_h = if panes.len() > 1 { crate::ui::CELL_TITLE_PT * scale } else { 0.0 };
    let layout = Layout::build(
        w as f64,
        h as f64,
        crate::ui::SIDEBAR_W_LOGICAL * scale,
        crate::HEADER_PT * scale,
        title_h,
        cols,
        rows,
        cell_w,
        cell_h,
    )
    .with_chrome_showing(scale, panes.len(), None, false);

    let grids: Vec<Grid> =
        (0..panes.len()).map(|i| (panes[i].content)(layout.cells[i].cols, layout.cells[i].rows)).collect();
    let views: Vec<SessionView> = panes
        .iter()
        .zip(&grids)
        .enumerate()
        .map(|(i, (p, grid))| SessionView {
            grid,
            view_offset: 0,
            cursor_visible: i == focused,
            focused: i == focused,
            title: p.title,
            selection: p.selection,
            ime_preedit: "",
            dormant: false,
            recede: 0,
            scrim: if i == focused { 0.0 } else { crate::frame_build::frame::UNFOCUSED_SCRIM },
            right_badge: p.badge,
            agent_tui: false,
            cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: p.hits,
            search_overlay: p.search.clone(),
            seq: 1 + i as u64,
        })
        .collect();
    let sidebar: Vec<SidebarEntry> =
        panes.iter().map(|p| SidebarEntry { label: p.title, state: p.state }).collect();
    renderer.set_window_focused(true);
    open(&mut renderer, &layout);

    let mut wr = WindowRender::new();
    // Twice: glyphs rasterised during the first frame land in the
    // atlas texture the second one samples.
    renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &sidebar, focused);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &sidebar, focused);
    let mut rgba = renderer.read_target(&target).expect("readback");
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let first = [rgba[0], rgba[1], rgba[2], rgba[3]];
    assert!(rgba.chunks_exact(4).any(|px| px != first), "{name}: the frame is one flat colour");

    std::fs::create_dir_all(dir).expect("mkdir bench/ui/snapshots");
    let path = dir.join(format!("{name}.png"));
    assert_snapshot_ssim(&rgba, &path, w, h, 0.98);
    write_snapshot_png(&path, &rgba, w, h);
}

fn four_panes() -> [PaneSpec; 4] {
    [
        pane("smix", "P1 3f9c2a1e", SessionState::Active),
        pane("devops", "", SessionState::Idle),
        pane("insight-mono", "P2 77d0e4b2", SessionState::Exited),
        pane("alt", "", SessionState::Idle),
    ]
}

/// The whole window: toolbar, sidebar with four panes, a two by two
/// grid with title strips and badges, the second pane in use, the
/// pointer in the fourth pane's sidebar row.
#[test]
fn chrome_snapshot_window() {
    snapshot("chrome_window", (1600, 1000), (2, 2), &four_panes(), 1, |renderer, _| {
        renderer.set_hover_chrome_btn(Some(ChromeHover::Row(3)));
    });
}

/// One pane with the search overlay open: a query, a match count and
/// three hits, the second of them picked.  The pointer is on the
/// sidebar's add button, and the only pane cannot be closed.
#[test]
fn chrome_snapshot_search() {
    let hit = |snippet: &str, is_focused| SearchHitView { snippet: snippet.to_string(), is_focused };
    let panes = [PaneSpec {
        search: Some(SearchOverlayView {
            query: "lazy dog".to_string(),
            query_cursor: 8,
            case_sensitive: true,
            counter: Some((2, 17)),
            hits: vec![
                hit("jumps over the lazy dog  0123456789", false),
                hit("fox jumps over the lazy dog  01234", true),
                hit("brown fox jumps over the lazy dog", false),
            ],
        }),
        ..pane("smix", "", SessionState::Active)
    }];
    snapshot("chrome_search", (1200, 640), (1, 1), &panes, 0, |renderer, _| {
        renderer.set_hover_chrome_btn(Some(ChromeHover::Add));
    });
}

/// The context menu over a two by two grid, the pointer on its second
/// row; one row disabled, one divider.  The process monitor's toolbar
/// button is the one last hovered.
#[test]
fn chrome_snapshot_menu() {
    snapshot("chrome_menu", (1200, 760), (2, 2), &four_panes(), 0, |renderer, layout| {
        let row = |label: &str, hint: &str, enabled| ContextMenuRow {
            label: label.to_string(),
            shortcut_hint: hint.to_string(),
            enabled,
            divider: false,
        };
        let divider = ContextMenuRow {
            label: String::new(),
            shortcut_hint: String::new(),
            enabled: false,
            divider: true,
        };
        renderer.set_hover_chrome_btn(Some(ChromeHover::Toolbar(2)));
        renderer.set_context_menu(Some(ContextMenuRender {
            scale: crate::ui::chrome_scale(),
            anchor_phys: (420.0, 150.0),
            top_inset: layout.top_inset,
            items: vec![
                row("Copy", "⌘C", false),
                row("Paste", "⌘V", true),
                row("Search", "⌘F", true),
                divider,
                row("Clear scrollback", "⌘K", true),
                row("Close pane", "⌘W", true),
            ],
            hovered_idx: Some(1),
        }));
    });
}

/// The pointer on a sidebar row's close button, in a window narrow
/// enough that a pane's name has to give way to its badge.
#[test]
fn chrome_snapshot_close() {
    let panes = [
        pane("smix", "P1 3f9c2a1e", SessionState::Active),
        pane("a-name-longer-than-the-pane-it-titles", "P2 77d0e4b2", SessionState::Idle),
    ];
    snapshot("chrome_close", (800, 300), (2, 1), &panes, 0, |renderer, _| {
        renderer.set_hover_chrome_btn(Some(ChromeHover::Close(1)));
    });
}

/// Two panes of terminal content, the upper one in use: the sixteen
/// colours, bold, dim, underline and reverse, a selection, a search
/// hit and a link, on the pane in use's ground and on the window's.
#[test]
fn chrome_snapshot_terminal() {
    let panes = [ansi_pane("colours"), ansi_pane("colours, not in use")];
    snapshot("chrome_terminal", (1600, 900), (1, 2), &panes, 0, |_, _| {});
}

crate::snapshot_kit::snapshots_in_other_themes! {
    chrome_snapshot_window_light, chrome_snapshot_window_contrast = chrome_snapshot_window;
    chrome_snapshot_search_light, chrome_snapshot_search_contrast = chrome_snapshot_search;
    chrome_snapshot_menu_light, chrome_snapshot_menu_contrast = chrome_snapshot_menu;
    chrome_snapshot_terminal_light, chrome_snapshot_terminal_contrast = chrome_snapshot_terminal;
}

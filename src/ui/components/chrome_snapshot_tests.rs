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

use super::density_kit;
use super::panel_snapshot_tests::terminal_text;
use crate::grid::{Cell, CellAttrs, Color, Grid};
use crate::render::{HighlightSpan, SelectionView};
use crate::layout::Layout;
use crate::render::{SearchHitView, SearchOverlayView, SessionView, SidebarEntry};
use crate::render_metal::{make_readback_texture, MetalRenderer, WindowRender};
use crate::session::SessionState;
use crate::snapshot_kit::{assert_snapshot_ssim, snapshot_mode_in, write_snapshot_png, UI_SNAPSHOT_DIR};
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::strings::{t, Msg};
use crate::ui::components::context_menu_view::MenuRender;
use crate::ui::components::{ContextMenu, MenuItem};

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
    if !density_kit::capturing() && snapshot_mode_in(std::path::Path::new(UI_SNAPSHOT_DIR)).is_none() {
        return;
    }
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    snapshot_on(&mut renderer, &mut WindowRender::new(), name, (w, h), (cols, rows), panes, focused, open);
}

/// [`snapshot`] on a renderer and a window that may have drawn before,
/// at another display scale.
#[allow(clippy::too_many_arguments)]
fn snapshot_on(
    renderer: &mut MetalRenderer,
    wr: &mut WindowRender,
    name: &str,
    (w, h): (u32, u32),
    (cols, rows): (usize, usize),
    panes: &[PaneSpec],
    focused: usize,
    open: impl FnOnce(&mut MetalRenderer, &Layout),
) {
    let name = &density_kit::scaled_name(&crate::snapshot_kit::themed(name));
    let (w, h) = density_kit::scaled_size(w, h);
    let dir = std::path::Path::new(UI_SNAPSHOT_DIR);
    let target = make_readback_texture(renderer.device(), w, h).expect("a readback texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    // The app's own numbers at the display scale in force.
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
    .with_chrome(scale, panes.len(), None);

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
    open(renderer, &layout);

    // Twice: glyphs rasterised during the first frame land in the
    // atlas texture the second one samples.
    renderer.render_layout_to_texture(wr, &target, &layout, &views, &sidebar, focused);
    renderer.render_layout_to_texture(wr, &target, &layout, &views, &sidebar, focused);
    if density_kit::deliver(name, renderer, &layout, panes.len()) {
        return;
    }
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
pub(super) fn chrome_snapshot_window() {
    snapshot("chrome_window", (1600, 1000), (2, 2), &four_panes(), 1, |renderer, _| {
        renderer.set_hover_chrome_btn(Some(ChromeHover::Row(3)));
    });
}

/// The window of `chrome_snapshot_window` with the pointer on `hover`,
/// on a renderer that may have drawn it before at another scale.
pub(super) fn window_on(renderer: &mut MetalRenderer, wr: &mut WindowRender, hover: Option<ChromeHover>) {
    snapshot_on(renderer, wr, "chrome_window", (1600, 1000), (2, 2), &four_panes(), 1, |renderer, _| {
        renderer.set_hover_chrome_btn(hover);
    });
}

/// One pane with the search overlay open: a query, a match count and
/// three hits, the second of them picked.  The pointer is on the
/// sidebar's add button, and the only pane cannot be closed.
#[test]
pub(super) fn chrome_snapshot_search() {
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
pub(super) fn chrome_snapshot_menu() {
    snapshot("chrome_menu", (1200, 760), (2, 2), &four_panes(), 0, |renderer, layout| {
        open_menu(renderer, layout, Some(1));
    });
}

/// `chrome_snapshot_menu` with the pointer on row `hovered`, on a
/// renderer of the caller's.
pub(super) fn menu_on(renderer: &mut MetalRenderer, wr: &mut WindowRender, hovered: Option<usize>) -> MenuRender {
    let mut state = None;
    snapshot_on(renderer, wr, "chrome_menu", (1200, 760), (2, 2), &four_panes(), 0, |renderer, layout| {
        state = Some(open_menu(renderer, layout, hovered));
    });
    state.expect("the scene was drawn")
}

fn open_menu(renderer: &mut MetalRenderer, layout: &Layout, hovered: Option<usize>) -> MenuRender {
    let row = |label: &str, hint: &str, enabled: bool| {
        let item = MenuItem::entry(label, 0).with_shortcut(hint);
        if enabled { item } else { item.disabled() }
    };
    renderer.set_hover_chrome_btn(Some(ChromeHover::Toolbar(2)));
    let scale = crate::ui::chrome_scale();
    let anchor = (420.0 * scale, 150.0 * scale);
    let mut state = MenuRender::new(
        anchor,
        layout.top_inset,
        vec![
            row(t(Msg::MenuCopy), "⌘C", false),
            row(t(Msg::MenuPaste), "⌘V", true),
            row(t(Msg::CmdFind), "⌘F", true),
            MenuItem::divider(),
            row(t(Msg::MenuClearScrollback), "⌘K", true),
            row(t(Msg::MenuClosePane), "⌘W", true),
        ],
    );
    // the pointer in the middle of the row it is said to be on
    state.pointer = hovered.map(|i| {
        let menu = ContextMenu::layout(
            layout.window_w, layout.window_h, scale, anchor.0, anchor.1, layout.top_inset, &state.items,
        );
        let r = menu.item_rects[i];
        (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
    });
    renderer.set_context_menu(Some(state.clone()));
    state
}

/// The pointer on a sidebar row's close button, in a window narrow
/// enough that a pane's name has to give way to its badge.
#[test]
pub(super) fn chrome_snapshot_close() {
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
pub(super) fn chrome_snapshot_terminal() {
    let panes = [ansi_pane("colours"), ansi_pane("colours, not in use")];
    snapshot("chrome_terminal", (1600, 900), (1, 2), &panes, 0, |_, _| {});
}

crate::snapshot_kit::snapshots_in_other_themes! {
    chrome_snapshot_window_light, chrome_snapshot_window_contrast = chrome_snapshot_window;
    chrome_snapshot_search_light, chrome_snapshot_search_contrast = chrome_snapshot_search;
    chrome_snapshot_menu_light, chrome_snapshot_menu_contrast = chrome_snapshot_menu;
    chrome_snapshot_terminal_light, chrome_snapshot_terminal_contrast = chrome_snapshot_terminal;
}

//! Offscreen pictures of the main window's own chrome, compared with
//! committed PNGs: the toolbar, the sidebar, the pane title strips and
//! seams, the search overlay and the context menu.
//!
//! Same machinery and the same way of recording and fetching as the
//! panel pictures (see `panel_snapshot_tests`); the tests are
//! `chrome_snapshot_{window,search,menu,close,terminal,scrollback,prompt_jump}` and the
//! baselines `bench/ui/snapshots/chrome_{window,search,menu,close,terminal,scrollback,prompt_jump}.png`.
//! `before/` beside them holds the first three scenes as they were
//! drawn before the chrome took its colours and sizes from the theme.
//! All but `close` are drawn again under the light and the
//! high-contrast theme, as `chrome_<scene>.{light,contrast}.png`.

use super::density_kit;
use super::panel_snapshot_tests::terminal_text;
use crate::grid::{Cell, Grid};
use crate::render::{HighlightSpan, HistoryView, SelectionView};
use crate::layout::Layout;
use crate::render::{SearchHitView, SearchOverlayView, SessionView, SidebarEntry};
use crate::render_metal::{make_readback_texture, MetalRenderer, WindowRender};
use crate::session::SessionState;
use crate::snapshot_kit::{assert_snapshot_ssim, snapshot_mode_in, write_snapshot_png, UI_SNAPSHOT_DIR};
use crate::frame_build::chrome::ChromeShape;
use crate::ui::components::chrome_target::ChromeTarget;
use crate::ui::strings::{t, Msg};
use crate::ui::components::context_menu_view::MenuRender;
use crate::ui::components::{ContextMenu, MenuItem};

#[cfg(test)]
#[path = "chrome_ansi_snapshot_tests.rs"]
mod ansi;
use ansi::ansi_scene;
#[cfg(test)]
#[path = "sidebar_tree_snapshot_tests.rs"]
mod sidebar_tree;
#[cfg(test)]
#[path = "menu_beside_snapshot_tests.rs"]
mod menu_beside;
#[cfg(test)]
#[path = "close_ask_snapshot_tests.rs"]
mod close_ask;

thread_local! {
    /// The sidebar rows of the next scene, when they are not one per pane.
    static ROWS: std::cell::RefCell<Option<Vec<SidebarEntry<'static>>>> = const { std::cell::RefCell::new(None) };
}

/// One pane of a scene.
pub(super) struct PaneSpec {
    title: &'static str,
    badge: &'static str,
    state: SessionState,
    search: Option<SearchOverlayView>,
    /// What the pane's grid holds, at its size in cells.
    content: fn(u16, u16) -> Grid,
    selection: Option<SelectionView>,
    hits: &'static [HighlightSpan],
    history: HistoryView,
    /// Rows the pane gives up to a strip.
    strip: u16,
    /// What the core shows over it, and the button of it the pointer is on.
    notice: Option<String>,
    notice_hover: Option<u8>,
}

pub(super) fn pane(title: &'static str, badge: &'static str, state: SessionState) -> PaneSpec {
    PaneSpec {
        title,
        badge,
        state,
        search: None,
        content: terminal_text,
        selection: None,
        hits: &[],
        history: HistoryView::default(),
        strip: 0,
        notice: None,
        notice_hover: None,
    }
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
pub(super) fn snapshot(
    name: &str,
    (w, h): (u32, u32),
    (cols, rows): (usize, usize),
    panes: &[PaneSpec],
    focused: usize,
    open: impl FnOnce(&mut MetalRenderer, &mut WindowRender, &ChromeShape<'_>),
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
    open: impl FnOnce(&mut MetalRenderer, &mut WindowRender, &ChromeShape<'_>),
) {
    let name = &density_kit::scaled_name(&crate::snapshot_kit::themed(name));
    let (w, h) = density_kit::scaled_size(w, h);
    let dir = std::path::Path::new(UI_SNAPSHOT_DIR);
    let target = make_readback_texture(renderer.device(), w, h).expect("a readback texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    // The app's own numbers at the display scale in force.
    let scale = crate::ui::chrome_scale();
    let title_h = if panes.len() > 1 { crate::ui::CELL_TITLE_PT * scale } else { 0.0 };
    // the codes of the panes in the screenshot the sidebar's design was drawn from
    const CODES: [u16; 5] = [4145, 5528, 8654, 6004, 8590];
    let sidebar: Vec<SidebarEntry> = ROWS.with(|r| r.borrow_mut().take()).unwrap_or_else(|| {
        panes
            .iter()
            .enumerate()
            .map(|(i, p)| SidebarEntry { selected: i == focused, ..SidebarEntry::top(p.title, p.state, Some(CODES[i % CODES.len()])) })
            .collect()
    });
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
    .with_chrome(scale, sidebar.len(), None)
    .with_strips(&panes.iter().map(|p| p.strip).collect::<Vec<_>>(), cell_w, cell_h);
    let strip_grids: Vec<Option<Grid>> = panes
        .iter()
        .zip(&layout.cells)
        .map(|(p, c)| (p.strip > 0).then(|| super::helper_strip_view_tests::strip_scene(c.cols, p.strip)))
        .collect();

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
            history: p.history, session: 0, notice: p.notice.as_deref().map(|text| crate::render::NoticeView { text, hover: p.notice_hover }),
            strip: strip_grids[i].as_ref().map(|strip| super::helper_strip_view_tests::strip_under(grid, strip)),
            seq: 1 + i as u64,
        })
        .collect();
    renderer.set_window_focused(true);
    let strips: &[SessionView] = &views;
    open(renderer, wr, &ChromeShape { layout: &layout, rows: sidebar.len(), strips: &strips });

    // Twice: glyphs rasterised during the first frame land in the
    // atlas texture the second one samples.
    renderer.render_layout_to_texture(wr, &target, &layout, &views, &sidebar, focused);
    renderer.render_layout_to_texture(wr, &target, &layout, &views, &sidebar, focused);
    if density_kit::deliver(name, renderer, wr, &layout, panes.len()) {
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

/// A point on `t` in a window laid out as `l`: the middle of a button,
/// the left of a row, well clear of its close button.
pub(super) fn point_on(l: &Layout, t: ChromeTarget) -> (f64, f64) {
    let mid = |r: crate::layout::Rect| (r.x + r.w * 0.5, r.y_top + r.h * 0.5);
    let row_h = marspot_term::layout::sidebar_row_h();
    match t {
        ChromeTarget::Toolbar(i) => mid(l.toolbar_buttons()[i as usize]),
        ChromeTarget::Add => mid(l.add_session_button_rect),
        ChromeTarget::Close(i) => mid(l.close_session_rects[i]),
        ChromeTarget::Row(i) => (l.sidebar_w * 0.25, l.top_inset + l.sidebar_top_pad_phys + (i as f64 + 0.5) * row_h),
        ChromeTarget::AllHelpers => (l.sidebar_w * 0.5, l.window_h - row_h * 0.5),
        ChromeTarget::Title(i) | ChromeTarget::Badge(i) | ChromeTarget::Helpers(i) => (l.cells[i].x + 4.0, l.cells[i].y_top + 1.0),
    }
}

/// Put the pointer on `t`.
fn hover(renderer: &mut MetalRenderer, wr: &mut WindowRender, shape: &ChromeShape<'_>, t: ChromeTarget) {
    wr.chrome_pointer_move(renderer.font_mut(), shape, point_on(shape.layout, t));
}

/// The whole window: toolbar, sidebar with four panes, a two by two
/// grid with title strips and badges, the second pane in use, the
/// pointer in the fourth pane's sidebar row.
#[test]
pub(super) fn chrome_snapshot_window() {
    snapshot("chrome_window", (1600, 1000), (2, 2), &four_panes(), 1, |renderer, wr, shape| {
        hover(renderer, wr, shape, ChromeTarget::Row(3));
    });
}

/// The window of `chrome_snapshot_window` with the pointer on `hover`,
/// on a renderer that may have drawn it before at another scale.
pub(super) fn window_on(renderer: &mut MetalRenderer, wr: &mut WindowRender, on: Option<ChromeTarget>) {
    snapshot_on(renderer, wr, "chrome_window", (1600, 1000), (2, 2), &four_panes(), 1, |renderer, wr, shape| {
        match on {
            Some(t) => hover(renderer, wr, shape, t),
            None => {
                wr.chrome_pointer_leave();
            }
        }
    });
}

/// The window of `chrome_snapshot_window` with `all` shadow panes, the
/// panes' own counted in `under`, on a renderer of the caller's.
pub(super) fn helpers_on(
    renderer: &mut MetalRenderer,
    wr: &mut WindowRender,
    (all, under): (u32, &[u32]),
    open: impl FnOnce(&mut MetalRenderer, &mut WindowRender, &ChromeShape<'_>),
) {
    snapshot_on(renderer, wr, "helpers_chrome", (1600, 1000), (2, 2), &four_panes(), 1, |renderer, wr, shape| {
        wr.set_helpers(all, under.iter().copied());
        open(renderer, wr, shape);
    });
}

pub(super) fn window_with_helpers(all: u32, under: &[u32]) {
    snapshot("helpers_chrome", (1600, 1000), (2, 2), &four_panes(), 1, |_, wr, _| {
        wr.set_helpers(all, under.iter().copied());
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
    snapshot("chrome_search", (1200, 640), (1, 1), &panes, 0, |renderer, wr, shape| {
        hover(renderer, wr, shape, ChromeTarget::Add);
    });
}

/// The context menu over a two by two grid, the pointer on its second
/// row; one row disabled, one divider.  The process monitor's toolbar
/// button is the one last hovered.
#[test]
pub(super) fn chrome_snapshot_menu() {
    snapshot("chrome_menu", (1200, 760), (2, 2), &four_panes(), 0, |renderer, wr, shape| {
        open_menu(renderer, wr, shape, Some(1));
    });
}

/// `chrome_snapshot_menu` with the pointer on row `hovered`, on a
/// renderer of the caller's.
pub(super) fn menu_on(renderer: &mut MetalRenderer, wr: &mut WindowRender, hovered: Option<usize>) -> MenuRender {
    let mut state = None;
    snapshot_on(renderer, wr, "chrome_menu", (1200, 760), (2, 2), &four_panes(), 0, |renderer, wr, shape| {
        state = Some(open_menu(renderer, wr, shape, hovered));
    });
    state.expect("the scene was drawn")
}

fn open_menu(
    renderer: &mut MetalRenderer,
    wr: &mut WindowRender,
    shape: &ChromeShape<'_>,
    hovered: Option<usize>,
) -> MenuRender {
    let layout = shape.layout;
    let row = |label: &str, hint: &str, enabled: bool| {
        let item = MenuItem::entry(label, 0).with_shortcut(hint);
        if enabled { item } else { item.disabled() }
    };
    hover(renderer, wr, shape, ChromeTarget::Toolbar(2));
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
    snapshot("chrome_close", (800, 300), (2, 1), &panes, 0, |renderer, wr, shape| {
        hover(renderer, wr, shape, ChromeTarget::Close(1));
    });
}

/// Two panes of terminal content, the upper one in use: the sixteen
/// colours, bold, dim, underline and reverse, a selection, a search
/// hit and a link, on the pane in use's ground and on the window's.
#[test]
pub(super) fn chrome_snapshot_terminal() {
    let panes = [ansi_pane("colours"), ansi_pane("colours, not in use")];
    snapshot("chrome_terminal", (1600, 900), (1, 2), &panes, 0, |_, _, _| {});
}

/// Two panes scrolled back into their history: the one in use a third
/// of the way up a short history, the other near the top of a deep one,
/// where the thumb is as short as it gets.
#[test]
pub(super) fn chrome_snapshot_scrollback() {
    let panes = [
        PaneSpec { history: HistoryView { offset: 40, len: 120 }, ..pane("smix", "", SessionState::Active) },
        PaneSpec { history: HistoryView { offset: 9000, len: 9500 }, ..pane("devops", "", SessionState::Idle) },
    ];
    snapshot("chrome_scrollback", (1200, 640), (2, 1), &panes, 0, |_, _, _| {});
}

/// A pane a prompt jump moved back into its history, the row it put on
/// top marked across the pane in the search hit's colours.
#[test]
pub(super) fn chrome_snapshot_prompt_jump() {
    let panes = [PaneSpec {
        history: HistoryView { offset: 40, len: 120 },
        hits: &[HighlightSpan { view_row: 0, col_start: 0, col_end_inclusive: u16::MAX }],
        ..pane("smix", "", SessionState::Active)
    }];
    snapshot("chrome_prompt_jump", (1200, 640), (1, 1), &panes, 0, |_, _, _| {});
}

/// A screen that ends a third of the way down.
fn short_text(cols: u16, rows: u16) -> Grid {
    let mut g = terminal_text(cols, rows);
    for row in rows / 3..rows {
        for col in 0..cols {
            g.set_cell(col, row, Cell::default());
        }
    }
    g
}

/// Panes that give up rows at the bottom: one whose screen is full, one
/// whose screen is not, and one without.
#[test]
pub(super) fn chrome_snapshot_helper_strip() {
    let panes = [
        PaneSpec { strip: 5, ..pane("smix", "P1 3f9c2a1e", SessionState::Active) },
        PaneSpec { strip: 5, content: short_text, ..pane("devops", "", SessionState::Idle) },
        pane("alt", "", SessionState::Idle),
        PaneSpec { strip: 3, content: short_text, ..pane("kevy", "", SessionState::Idle) },
    ];
    snapshot("chrome_helper_strip", (1600, 1000), (2, 2), &panes, 0, |_, _, _| {});
}

crate::snapshot_kit::snapshots_in_other_themes! {
    chrome_snapshot_helper_strip_light, chrome_snapshot_helper_strip_contrast = chrome_snapshot_helper_strip;
    chrome_snapshot_window_light, chrome_snapshot_window_contrast = chrome_snapshot_window;
    chrome_snapshot_search_light, chrome_snapshot_search_contrast = chrome_snapshot_search;
    chrome_snapshot_menu_light, chrome_snapshot_menu_contrast = chrome_snapshot_menu;
    chrome_snapshot_terminal_light, chrome_snapshot_terminal_contrast = chrome_snapshot_terminal;
}

/// The question asked over the focused pane before an agent CLI's
/// first install, in a window of two panes.
#[test]
pub(super) fn chrome_snapshot_install_ask() {
    use crate::ui::strings::{text, Arg};
    let ask = text(Arg::InstallAsk { cli: "codex", place: "~/.codex/packages + ~/.local/bin", megabytes: 740 });
    let panes = [PaneSpec { notice: Some(ask), ..pane("smix", "", SessionState::Active) }, pane("devops", "", SessionState::Idle)];
    snapshot("chrome_install_ask", (1200, 640), (2, 1), &panes, 0, |_, _, _| {});
}

#[test]
fn chrome_snapshot_install_ask_2x() {
    density_kit::at_scale(2.0, chrome_snapshot_install_ask);
}

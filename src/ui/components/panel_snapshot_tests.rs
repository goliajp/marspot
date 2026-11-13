//! One offscreen picture per panel, compared with a committed PNG.
//!
//! The panels are meant to look like one program: same header,
//! same paddings, same controls.  These tests draw each of them over a
//! terminal full of text, with fixed data, so the pictures can be
//! put side by side and so a change to one panel shows up as a changed
//! picture rather than as a remark weeks later.
//!
//! Baselines are `bench/ui/snapshots/<panel>.png`; `before/` beside
//! them holds how each panel looked before it was moved onto the shared
//! chrome, for reference only (nothing compares against it).
//!
//! A scene's size is given in pixels at display scale 1, where a
//! logical point is two pixels (`ViewPainter::px_per_pt()`); at scale 2
//! the window is twice that and the baseline is `<panel>.2x.png`.
//!
//! ## Working on one panel
//!
//! Tests build and run on the remote runner, never on the machine
//! someone is typing on.  From the repository root:
//!
//! ```text
//! # draw it, overwriting the baseline on the runner, and bring the
//! # picture back to look at
//! export REMOTE_NAME=test-marspot-settings
//! MARSPOT_FONT_SNAPSHOT=1 bin/test-remote.sh -E 'test(panel_snapshot_settings)' \
//!   && scp mini:work/$REMOTE_NAME/bench/ui/snapshots/settings.png bench/ui/snapshots/
//! # compare with the committed baseline instead (what the suite does)
//! bin/test-remote.sh -E 'test(panel_snapshot)'
//! ```
//!
//! `REMOTE_NAME` gives the run its own tree on the runner
//! (`~/work/<name>`).  Without it every checkout syncs into the same
//! one, and a sync deletes what is not in the tree it came from: a
//! PNG recorded there is gone as soon as another checkout runs its
//! tests.  Only one `test-remote.sh` runs at a time per machine, so a
//! second one started meanwhile exits at once and has to be retried.
//!
//! The names are
//! `panel_snapshot_{settings,layout,process,accounts,shortcuts}` and
//! the files `{settings,layout,process,accounts,shortcuts}.png`;
//! `panel_snapshot_accounts_narrow` / `accounts_narrow.png` is the
//! accounts panel in a window where its cards take two rows, and
//! `panel_snapshot_accounts_empty` / `accounts_empty.png` the same
//! panel before any usage has been read;
//! `panel_snapshot_layout_grid` / `layout_grid.png` is the layout
//! panel at three by three, mid-drag, with a name too long for a card;
//! `panel_snapshot_controls` / `controls.png` is every shared control
//! in every state, in a panel drawn by the shared chrome.  A check
//! that fails keeps its render only where `MARSPOT_SNAPSHOT_OUT` names
//! a directory; recording is the way to see one.
//!
//! Six of them are drawn again under the light and the high-contrast
//! theme: `panel_snapshot_<panel>_{light,contrast}` and
//! `<panel>.{light,contrast}.png`.

use crate::grid::{Cell, Grid};
use crate::layout::Layout;
use crate::render::SessionView;
use crate::render_metal::{make_readback_texture, MetalRenderer, WindowRender};
use crate::snapshot_kit::{assert_snapshot_ssim, snapshot_mode_in, write_snapshot_png, UI_SNAPSHOT_DIR};
use crate::ui::components::cc_usage_modal::{
    self, CcUsageAccountRender, CcUsageRender, CcUsageWindowRender, Clock,
};
use crate::ui::components::controls;
use crate::ui::components::density_kit;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::core::ViewPainter;
use crate::ui::strings::{self, t, Arg, MonthDayTime, Msg};
use crate::ui::theme::spec;
use marspot_term::layout::Rect;
use crate::ui::components::layout_modal::{self, LayoutModal};
use crate::ui::components::layout_view::{LayoutModalDragRender, LayoutModalRender};
use crate::ui::components::process_panel::{
    self, ProcessPanelPaneRow, ProcessPanelRender, ProcessPanelRow,
};
use crate::ui::components::settings_modal;
use crate::ui::components::settings_view::SettingsRender;

/// A terminal's worth of text for the panel to sit on, so the dimmed
/// backdrop and the frame's shadow are in the picture.
pub(super) fn terminal_text(cols: u16, rows: u16) -> Grid {
    const LINE: &str = "the quick brown fox jumps over the lazy dog  0123456789  ";
    let mut g = Grid::new(cols, rows);
    for r in 0..rows {
        for (c, ch) in LINE.chars().cycle().skip(r as usize * 7).take(cols as usize).enumerate() {
            g.set_cell(c as u16, r, Cell { ch, attrs: Default::default() });
        }
    }
    g
}

/// Draw one frame of a `w × h` px window with whatever `open` put on
/// the renderer, and hold it to `bench/ui/snapshots/<name>.png`.
pub(crate) fn snapshot(name: &str, w: u32, h: u32, open: impl FnOnce(&mut MetalRenderer, &Layout)) {
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
    snapshot_on(&mut renderer, &mut WindowRender::new(), name, w, h, open);
}

/// [`snapshot`] on a renderer and a window that may have drawn before,
/// at another display scale.
pub(super) fn snapshot_on(
    renderer: &mut MetalRenderer,
    wr: &mut WindowRender,
    name: &str,
    w: u32,
    h: u32,
    open: impl FnOnce(&mut MetalRenderer, &Layout),
) {
    let name = &density_kit::scaled_name(&crate::snapshot_kit::themed(name));
    let (w, h) = density_kit::scaled_size(w, h);
    let dir = std::path::Path::new(UI_SNAPSHOT_DIR);
    let target = make_readback_texture(renderer.device(), w, h).expect("a readback texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(w as f64, h as f64, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
    let grid = terminal_text(
        ((w as f64 / cell_w) as u16).max(8),
        ((h as f64 / cell_h) as u16).max(4),
    );
    let view = SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: false,
        focused: true,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "",
        agent_tui: false,
        cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        seq: 1,
    };
    open(renderer, &layout);

    let views = std::slice::from_ref(&view);
    // Twice: glyphs rasterised during the first frame land in the
    // atlas texture the second one samples.
    renderer.render_layout_to_texture(wr, &target, &layout, views, &[], 0);
    renderer.render_layout_to_texture(wr, &target, &layout, views, &[], 0);
    if density_kit::deliver(name, renderer, &layout, 1) {
        return;
    }
    let mut rgba = renderer.read_target(&target).expect("readback");
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    if density_kit::deliver_pixels(&rgba, w, h) {
        return;
    }
    // A picture of nothing passes any comparison with a picture of
    // nothing, so the frame has to have something in it.
    let first = [rgba[0], rgba[1], rgba[2], rgba[3]];
    assert!(
        rgba.chunks_exact(4).any(|px| px != first),
        "{name}: the frame is one flat colour",
    );

    std::fs::create_dir_all(dir).expect("mkdir bench/ui/snapshots");
    let path = dir.join(format!("{name}.png"));
    assert_snapshot_ssim(&rgba, &path, w, h, 0.98);
    write_snapshot_png(&path, &rgba, w, h);
}

#[test]
pub(super) fn panel_snapshot_settings() {
    snapshot("settings", 1000, 1360, |renderer, layout| {
        open_settings(renderer, layout, None);
    });
}

/// The settings panel as the picture has it, with the pointer at
/// `pointer`.  Returns what it was drawn from, its surface included.
pub(super) fn open_settings(
    renderer: &mut MetalRenderer,
    layout: &Layout,
    pointer: Option<(f64, f64)>,
) -> SettingsRender {
    {
        let settings = crate::settings::Settings {
            reclaim_enabled: false,
            reclaim_prefetch: false,
            appearance_circled_wide: false,
            dim_scale: 1.0,
            scroll_factor: 1.0,
            cc_statusline_hook: true,
            // The Theme row shows the theme the picture is drawn in.
            appearance_theme: crate::settings::get().appearance_theme,
            appearance_language: crate::settings::get().appearance_language,
            ..Default::default()
        };
        let rect = {
            let mut measure = controls::font_measure(renderer.font_mut());
            settings_modal::panel_rect(
                layout.window_w, layout.window_h, &settings, &mut measure, layout.top_inset,
            )
        };
        let path = "/Users/you/Library/Application Support/marspot/settings.toml";
        let sp = SettingsRender { pointer, ..SettingsRender::new(rect, 0.0, settings, path) };
        renderer.set_settings_panel(Some(sp.clone()));
        sp
    }
}

#[test]
pub(super) fn panel_snapshot_shortcuts() {
    snapshot("shortcuts", 1600, 1200, |renderer, layout| {
        let rect = crate::ui::components::shortcuts_panel::panel_rect(
            layout.window_w, layout.window_h, renderer.cell_dims(), layout.top_inset,
        );
        renderer.set_shortcuts_panel(Some(crate::ui::components::shortcuts_view::ShortcutsRender::new(rect)));
    });
}

#[test]
pub(super) fn panel_snapshot_layout() {
    snapshot("layout", 1000, 760, |renderer, layout| {
        open_layout(renderer, layout, None);
    });
}

/// The layout panel as the picture has it, with the pointer at
/// `pointer`.  Returns what it was drawn from, its surface included.
pub(super) fn open_layout(
    renderer: &mut MetalRenderer,
    _layout: &Layout,
    pointer: Option<(f64, f64)>,
) -> LayoutModalRender {
    let slot_titles = ["smix", "devops", "insight-mono", "alt"].map(String::from).to_vec();
    let state = LayoutModalRender { cols: 4, rows: 1, slot_titles, pointer, ..Default::default() };
    renderer.set_layout_modal(Some(state.clone()));
    state
}

/// Three by three with a name no card can hold, mid-drag: the cut
/// label, the hole the dragged card left, the slot it would land in
/// and the card under the pointer.
#[test]
pub(super) fn panel_snapshot_layout_grid() {
    snapshot("layout_grid", 1600, 900, |renderer, layout| {
        let slot_titles = [
            "smix", "lab38-golialab-experiments-with-a-very-long-directory-name-that-keeps-on-going", "insight-mono",
            "alt", "devops", "marspot",
            "torajs", "", "",
        ]
        .map(String::from)
        .to_vec();
        let modal = {
            let mut m = controls::font_measure(renderer.font_mut());
            let label_w = layout_modal::widest_label(&slot_titles, &mut m);
            LayoutModal::layout(layout.window_w, layout.window_h, layout.top_inset, 3, 3, label_w, &mut m)
        };
        // The first card, held by its middle, most of the way onto the
        // sixth slot.
        let (from, over) = (modal.cards[0], modal.cards[5]);
        let grab = (from.w * 0.5, from.h * 0.5);
        renderer.set_layout_modal(Some(LayoutModalRender {
            cols: 3,
            rows: 3,
            slot_titles,
            drag: Some(LayoutModalDragRender {
                from_slot: 0,
                grab_offset_phys: grab,
                mouse_phys: (over.x - over.w * 0.2 + grab.0, over.y_top + over.h * 0.4 + grab.1),
            }),
            ..Default::default()
        }));
    });
}

#[test]
pub(super) fn panel_snapshot_process() {
    // The pointer on one kill button, so the picture has the hovered
    // row and the hovered button in it.
    snapshot("process", 1600, 1000, |renderer, layout| {
        open_process(renderer, layout, process_panel::Hover::Kill(6));
    });
}

/// The process monitor as the picture has it, with the pointer on
/// `hover`.  Returns what it was drawn from, its surface included.
pub(super) fn open_process(
    renderer: &mut MetalRenderer,
    layout: &Layout,
    hover: process_panel::Hover,
) -> ProcessPanelRender {
    {
        let pane = |name: &str, sid: u64, n_pids: u32, cpu_pct: f32, rss_mb: u64| ProcessPanelPaneRow {
            name: name.to_string(),
            sid,
            n_pids,
            cpu_pct,
            rss_kb: rss_mb * 1024,
            busy: String::new(),
        };
        let row = |depth: u8, pid: i32, comm: &str, cpu_pct: f32, rss_mb: u64| ProcessPanelRow {
            depth,
            pid,
            comm: comm.to_string(),
            cpu_pct,
            rss_kb: rss_mb * 1024,
        };
        let pane_rows = vec![
            pane("smix", 479, 10, 14.6, 682),
            pane("devops", 480, 3, 0.4, 345),
            pane("insight-mono", 481, 3, 0.0, 527),
            pane("alt", 482, 2, 0.0, 69),
        ];
        let rows = vec![
            row(0, 1544, "zsh", 0.0, 2),
            row(1, 26121, "2.1.322", 2.3, 491),
            row(2, 26409, "smix-mcp", 0.0, 6),
            row(2, 42371, "sourcekit-lsp", 0.0, 23),
            row(2, 47273, "caffeinate", 0.0, 2),
            row(2, 49143, "rust-analyzer", 0.2, 34),
            row(3, 49169, "rust-analyzer-p", 12.1, 50),
            row(3, 49170, "rust-analyzer-p", 0.0, 52),
            row(2, 60271, "zsh", 0.0, 3),
            row(3, 61997, "smix", 0.0, 21),
        ];
        let (cell_w, cell_h) = renderer.cell_dims();
        let frame = process_panel::panel_rect(
            layout.window_w, layout.window_h, cell_h, layout.top_inset, (0.0, 0.0),
        );
        let l = process_panel::layout(
            frame, layout.top_inset, pane_rows.len(), rows.len(), 0.0, cell_w, cell_h,
        );
        let middle = |r: Rect| (r.x + r.w * 0.5, r.y_top + r.h * 0.5);
        let row_of = |rows: &[(usize, Rect)], i: usize| rows.iter().find(|(n, _)| *n == i).map(|(_, r)| *r);
        // off the kill button at the row's far end
        let in_row = |r: Rect| (r.x + r.w * 0.25, r.y_top + r.h * 0.5);
        let pointer = match hover {
            process_panel::Hover::None => None,
            process_panel::Hover::Close => Some(middle(l.chrome.close)),
            process_panel::Hover::Pane(i) => row_of(&l.panes.rows, i).map(in_row),
            process_panel::Hover::Process(i) => row_of(&l.processes.rows, i).map(in_row),
            process_panel::Hover::Kill(i) => row_of(&l.kills, i).map(middle),
        };
        let panel = ProcessPanelRender {
            layout: l,
            pane_rows,
            selected_pane: 0,
            detail: strings::text(Arg::ProcessDetail { pane: "smix", sid: 479, shell_pid: 1544 }),
            rows,
            pointer,
            surface: Default::default(),
        };
        renderer.set_process_panel(Some(panel.clone()));
        panel
    }
}

/// 2026-11-10 09:00 UTC, so the timeline lands in the same place
/// every run.
const ACCOUNTS_NOW: i64 = 1_794_301_200;
/// The time zone the accounts panel's dates are read in: nine hours
/// east, where 09:00 UTC is the 18:00 of the "updated" line.  Handed
/// to the panel rather than taken from the machine, so the day rules
/// and their dates are the same picture wherever the test runs.
const ACCOUNTS_UTC_OFFSET: i64 = 9 * 3_600;

/// `(label, utilisation, hours until reset, reset clock time)`.
type Window = (&'static str, f32, i64, &'static str);

fn account(
    name: &str,
    email: &str,
    limited: bool,
    reset: (u32, u32, u32, u32),
    windows: &[Window],
) -> CcUsageAccountRender {
    let (month, day, hour, minute) = reset;
    CcUsageAccountRender {
        name: name.to_string(),
        email: email.to_string(),
        status_label: t(if limited { Msg::StatusLimit } else { Msg::StatusOk }).to_string(),
        status_severity: if limited { 2 } else { 0 },
        reset_label: strings::text(Arg::ResetAt(MonthDayTime { month, day, hour, minute })),
        windows: windows
            .iter()
            .map(|&(label, util, hours, hm)| CcUsageWindowRender {
                label: label.to_string(),
                util,
                reset_unix: Some(ACCOUNTS_NOW + hours * 3600),
                span_secs: 7 * 86_400,
                reset_hm: hm.to_string(),
            })
            .collect(),
    }
}

/// Ten accounts, some at their limit, one with an address too long
/// for its card.
fn accounts_panel(renderer: &mut MetalRenderer, layout: &Layout) {
    open_accounts(renderer, layout, None);
}

/// What the accounts pictures show: ten accounts, when the feed was
/// written, and the moment it is read at.
pub(crate) fn accounts_data() -> (Vec<CcUsageAccountRender>, String, Clock) {
    let accounts = vec![
        account("Claude 1", "dev@example.com", true, (11, 13, 11, 59),
            &[("7d", 1.0, 66, "11:59"), ("fable", 0.91, 66, "11:59")]),
        account("Claude 2", "admin@example.com", false, (11, 16, 6, 0),
            &[("7d", 0.35, 132, "06:00"), ("fable", 0.28, 132, "06:00")]),
        account("Claude 3", "ops@example.com", true, (11, 15, 13, 59),
            &[("7d", 1.0, 116, "13:59"), ("fable", 0.72, 116, "13:59")]),
        account("Claude 4", "claude@example.com", true, (11, 14, 0, 0),
            &[("7d", 1.0, 78, "00:00"), ("fable", 0.82, 78, "00:00")]),
        account("Claude 5", "lab@example.com", true, (11, 12, 10, 59),
            &[("7d", 1.0, 41, "10:59"), ("fable", 0.33, 41, "10:59")]),
        account("Claude 6", "ceo@example.com", true, (11, 13, 13, 59),
            &[("7d", 1.0, 68, "13:59"), ("fable", 0.94, 68, "13:59")]),
        account("Claude 7", "research.platform.team@example.com", false, (11, 17, 13, 0),
            &[("7d", 0.0, 163, "13:00"), ("fable", 0.0, 163, "13:00")]),
        account("Claude 8", "manage@example.com", true, (11, 11, 13, 59),
            &[("7d", 1.0, 20, "14:00"), ("fable", 0.47, 20, "13:59")]),
        account("Codex 1", "dev@example.com", true, (11, 12, 5, 2),
            &[("astra", 1.0, 35, "05:02")]),
        account("Codex 2", "ops@example.com", false, (11, 16, 3, 8),
            &[("astra", 0.64, 129, "03:08")]),
    ];
    let updated = strings::text(Arg::UpdatedAt(MonthDayTime { month: 11, day: 10, hour: 18, minute: 0 }));
    (accounts, updated, Clock::fixed(ACCOUNTS_NOW, ACCOUNTS_UTC_OFFSET))
}

/// The accounts panel as the picture has it, with the pointer at
/// `pointer`.  Returns the panel's frame and its surface.
pub(super) fn open_accounts(
    renderer: &mut MetalRenderer,
    layout: &Layout,
    pointer: Option<(f64, f64)>,
) -> (Rect, std::rc::Rc<std::cell::RefCell<crate::ui::components::cc_usage_view::CcUsageSurface>>) {
    let (accounts, updated_label, clock) = accounts_data();
    let (cell_w, cell_h) = renderer.cell_dims();
    let rect = cc_usage_modal::panel_rect(
        accounts.len(), 2, layout.window_w, layout.window_h, cell_w, cell_h, layout.top_inset,
    );
    let surface = std::rc::Rc::<std::cell::RefCell<crate::ui::components::cc_usage_view::CcUsageSurface>>::default();
    renderer.set_cc_usage(Some(CcUsageRender {
        rect,
        updated_label,
        accounts,
        clock,
        pointer,
        surface: surface.clone(),
    }));
    (rect, surface)
}

/// Wide enough for the ten cards to sit in one row.
#[test]
pub(super) fn panel_snapshot_accounts() {
    snapshot("accounts", 2800, 1300, accounts_panel);
}

/// The same panel where one row would make the cards too narrow: two
/// rows of five.
#[test]
pub(super) fn panel_snapshot_accounts_narrow() {
    snapshot("accounts_narrow", 1700, 1500, accounts_panel);
}

/// Before either feed has been read: the header, the close button
/// under the pointer, and one sentence.
#[test]
pub(super) fn panel_snapshot_accounts_empty() {
    snapshot("accounts_empty", 1700, 700, |renderer, layout| {
        let (cell_w, cell_h) = renderer.cell_dims();
        let rect = cc_usage_modal::panel_rect(
            0, 0, layout.window_w, layout.window_h, cell_w, cell_h, layout.top_inset,
        );
        // the pointer on the close button
        let close = PanelChrome::new(rect, 0.0).close;
        renderer.set_cc_usage(Some(CcUsageRender {
            rect,
            updated_label: String::new(),
            accounts: Vec::new(),
            clock: Clock::fixed(ACCOUNTS_NOW, ACCOUNTS_UTC_OFFSET),
            pointer: Some((close.x + close.w * 0.5, close.y_top + close.h * 0.5)),
            surface: Default::default(),
        }));
    });
}

/// Every shared control in every state, in a panel drawn by the
/// shared chrome: the one picture where a secondary button, a stepper
/// at its limit and a hovered close button can be seen at all.
pub(crate) fn controls_sheet(p: &mut ViewPainter<'_>) {
    use controls::{ButtonKind, StepperPart};
    let frame = panel_chrome::frame_rect(
        p.window_w, p.window_h, spec::px(640.0), panel_chrome::frame_height(spec::px(268.0)), 0.0,
    );
    PanelChrome::new(frame, 0.0).paint(p, "Controls", Some("updated 11/10 18:00"), true, |p, content| {
        let gap = spec::px(spec::space::CONTROL_TO_CONTROL);
        let pad_x = spec::px(spec::card::ROW_PAD_X);
        let pad_y = spec::px(spec::card::ROW_PAD_Y);
        // One group: heading, then a card one row tall.  Returns the
        // card's inner left edge, the row's top, and where the next
        // group starts.
        let group = |p: &mut ViewPainter<'_>, top: f64, title: &str, trailing: Option<&str>, row_h: f64| {
            let card_top = panel_chrome::section_heading(p, content.x, top, content.w, title, trailing);
            let card = Rect { x: content.x, y_top: card_top, w: content.w, h: row_h + 2.0 * pad_y };
            panel_chrome::card(p, card);
            (card.x + pad_x, card.y_top + pad_y, card.y_top + card.h + spec::px(spec::space::CARD_TO_HEADING))
        };

        let (w, h) = controls::button_size("Apply", &mut controls::font_measure(&mut *p.font));
        let (mut x, y, next) = group(p, content.y_top, "Buttons", Some("rest · hover · disabled"), h);
        for (label, kind, hovered, disabled) in [
            ("Apply", ButtonKind::Primary, false, false),
            ("Apply", ButtonKind::Primary, true, false),
            ("Apply", ButtonKind::Primary, false, true),
            ("Cancel", ButtonKind::Secondary, false, false),
            ("Cancel", ButtonKind::Secondary, true, false),
            ("Cancel", ButtonKind::Secondary, false, true),
        ] {
            controls::paint_button(p, Rect { x, y_top: y, w, h }, label, kind, hovered, disabled);
            x += w + gap;
        }

        let labels = [Msg::DimOff, Msg::DimLight, Msg::DimNormal, Msg::DimDeep];
        let (w, h) = controls::segmented_size(&labels, &mut controls::font_measure(&mut *p.font));
        let (x, y, next) = group(p, next, "Segmented control", Some("selected · hover · disabled"), h);
        controls::paint_segmented(p, Rect { x, y_top: y, w, h }, &labels, Some(2), Some(0), false);
        let x = x + w + 4.0 * gap;
        controls::paint_segmented(p, Rect { x, y_top: y, w, h }, &labels, Some(2), None, true);

        let (sw, sh) = controls::stepper_size();
        let (tw, th) = controls::toggle_size();
        let (mut x, y, next) = group(p, next, "Toggle and stepper", None, sh);
        for (on, disabled) in [(false, false), (true, false), (false, true), (true, true)] {
            let rect = Rect { x, y_top: y + (sh - th) * 0.5, w: tw, h: th };
            controls::paint_toggle(p, rect, on, disabled);
            x += tw + gap;
        }
        x += 3.0 * gap;
        controls::paint_stepper(p, x, y, "4", true, true, Some(StepperPart::Plus));
        controls::paint_stepper(p, x + sw + 4.0 * gap, y, "1", false, true, None);

        let (dw, dh) = controls::danger_button_size();
        let (mut x, y, _) = group(p, next, "Status and danger", Some("rest · hover"), dh);
        for (label, severity) in [("OK", 0), ("NEAR", 1), ("LIMIT", 2)] {
            let (w, h) = controls::chip_size(label, &mut controls::font_measure(&mut *p.font));
            controls::paint_chip(p, Rect { x, y_top: y + (dh - h) * 0.5, w, h }, label, severity);
            x += w + gap;
        }
        x += 3.0 * gap;
        controls::paint_danger_button(p, Rect { x, y_top: y, w: dw, h: dh }, false);
        controls::paint_danger_button(p, Rect { x: x + dw + gap, y_top: y, w: dw, h: dh }, true);
    });
}

#[test]
pub(super) fn panel_snapshot_controls() {
    snapshot("controls", 1400, 900, |_, _| {
        *crate::frame_build::frame::TEST_OVERLAY.lock().unwrap() = Some(controls_sheet);
    });
}

crate::snapshot_kit::snapshots_in_other_themes! {
    panel_snapshot_settings_light, panel_snapshot_settings_contrast = panel_snapshot_settings;
    panel_snapshot_layout_light, panel_snapshot_layout_contrast = panel_snapshot_layout;
    panel_snapshot_process_light, panel_snapshot_process_contrast = panel_snapshot_process;
    panel_snapshot_accounts_light, panel_snapshot_accounts_contrast = panel_snapshot_accounts;
    panel_snapshot_shortcuts_light, panel_snapshot_shortcuts_contrast = panel_snapshot_shortcuts;
    panel_snapshot_controls_light, panel_snapshot_controls_contrast = panel_snapshot_controls;
}

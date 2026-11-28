//! `--bench chrome-render:<n>` — a frame of a whole window: nine panes
//! with their title strips and badges, the sidebar with nine rows, the
//! toolbar and the seams, none of it changing between frames.
//!
//! `--bench render` draws one pane with no chrome at all, so it cannot
//! see what the window's chrome costs.  Here the panes come from their
//! caches every frame and what is left of the build is the chrome:
//! `chrome_p50_us` is the build less the pane loop.

use marspot::layout::Layout;
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::{make_target_texture, MetalRenderer, WindowRender};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;
use marspot::session::SessionState;
use marspot::terminal::Terminal;

pub(crate) const NAMES: [(&str, &str); 9] = [
    ("smix", "P1 3f9c2a1e"),
    ("devops", ""),
    ("insight-mono", "P2 77d0e4b2"),
    ("alt", ""),
    ("marspot", "P3 0c41aa90"),
    ("torajs-lane-biz", ""),
    ("mailrs", "P4 9e02b7c3"),
    ("sentori", ""),
    ("kevy", ""),
];

pub fn run(arg: &str) {
    let n: u32 = arg.parse().unwrap_or_else(|_| {
        eprintln!("bench: chrome-render needs an integer iteration count");
        std::process::exit(2);
    });
    let (mut renderer, target, layout, terminals) = scene();
    let views = views(&terminals);
    let sidebar = sidebar();
    let focused = FOCUSED;

    let mut wr = WindowRender::new();
    for _ in 0..5 {
        renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &sidebar, focused);
    }
    let mut total: Vec<u64> = Vec::with_capacity(n as usize);
    let mut build: Vec<u64> = Vec::with_capacity(n as usize);
    let mut panes: Vec<u64> = Vec::with_capacity(n as usize);
    let mut chrome: Vec<u64> = Vec::with_capacity(n as usize);
    let mut rebuilt = 0u64;
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &sidebar, focused);
        total.push(t0.elapsed().as_nanos() as u64);
        let sp = renderer.last_render_split();
        build.push(sp.build_us);
        panes.push(sp.build_panes_us);
        chrome.push(sp.build_us.saturating_sub(sp.build_panes_us));
        rebuilt += sp.panes_rebuilt as u64;
    }
    let pct = |v: &mut Vec<u64>, q: f64| -> u64 {
        v.sort_unstable();
        v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
    };
    println!(
        r#"{{"mode":"chrome-render","iterations":{},"p50_ns":{},"p99_ns":{},"build_p50_us":{},"build_p99_us":{},"panes_p50_us":{},"chrome_p50_us":{},"chrome_p99_us":{},"panes_rebuilt":{}}}"#,
        n,
        pct(&mut total, 0.50),
        pct(&mut total, 0.99),
        pct(&mut build, 0.50),
        pct(&mut build, 0.99),
        pct(&mut panes, 0.50),
        pct(&mut chrome, 0.50),
        pct(&mut chrome, 0.99),
        rebuilt,
    );
}

/// The renderer, a target the size of the window, the window's layout
/// and a terminal per pane with a screen of text.
#[allow(clippy::type_complexity)]
pub(crate) fn scene() -> (MetalRenderer, Retained<ProtocolObject<dyn MTLTexture>>, Layout, Vec<Terminal>) {
    marspot::ui::set_chrome_scale(1.0);
    let mut renderer = MetalRenderer::new_headless().expect("headless metal renderer");
    let (phys_w, phys_h) = (1800u32, 1100u32);
    let target = make_target_texture(renderer.device(), phys_w, phys_h).expect("render-target texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(
        phys_w as f64,
        phys_h as f64,
        marspot::ui::SIDEBAR_W_LOGICAL,
        marspot::HEADER_PT,
        marspot::ui::CELL_TITLE_PT,
        3,
        3,
        cell_w,
        cell_h,
    )
    .with_chrome(1.0, NAMES.len(), None);
    let terminals: Vec<Terminal> = (0..NAMES.len())
        .map(|i| {
            let cell = &layout.cells[i];
            let mut t = Terminal::new(cell.cols, cell.rows);
            let mut text = String::new();
            for r in 0..cell.rows.saturating_sub(1) {
                text.push_str(&format!("\x1b[{}m{i}:{r} the quick brown fox jumps over the lazy dog\r\n", 31 + r % 7));
            }
            t.feed(text.as_bytes());
            t
        })
        .collect();
    renderer.set_window_focused(true);
    (renderer, target, layout, terminals)
}

/// A view per pane, the fifth in use.
pub(crate) fn views(terminals: &[Terminal]) -> Vec<SessionView<'_>> {
    let focused = FOCUSED;
    terminals
        .iter()
        .enumerate()
        .map(|(i, t)| SessionView {
            grid: t.grid(),
            view_offset: 0,
            cursor_visible: i == focused,
            focused: i == focused,
            title: NAMES[i].0,
            selection: None,
            ime_preedit: "",
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: NAMES[i].1,
            agent_tui: false,
            cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(), session: 0, strip: None, notice: None,
            seq: 1 + i as u64,
        })
        .collect()
}

/// The pane in use in the bench's window.
pub(crate) const FOCUSED: usize = 4;

/// A sidebar row per pane.
pub(crate) fn sidebar() -> Vec<SidebarEntry<'static>> {
    let states = [SessionState::Active, SessionState::Idle, SessionState::Exited];
    NAMES
        .iter()
        .enumerate()
        .map(|(i, (label, _))| SidebarEntry {
            selected: i == FOCUSED,
            ..SidebarEntry::top(label, states[i % 3], Some(4100 + i as u16))
        })
        .collect()
}

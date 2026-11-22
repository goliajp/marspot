// CARVE-OUT: pre-existing file: the headless developer tools binary; shrink on every touch
//! The `marspot` binary: windowless developer tools.
//!
//! The product is `marspot-shell`, `marspot-core` and `marspot-session`;
//! this binary ships in none of them.  It carries the headless benches
//! the gate runs (`--bench <mode>:<arg>`) and, behind the `snapshot`
//! cargo feature, offscreen panel screenshots (`--snapshot <png>
//! [--panel <name>]`).

use marspot::layout::Layout;
use marspot::render::SessionView;
use marspot::render_metal::{make_target_texture, MetalRenderer, WindowRender};
use marspot::terminal::Terminal;
#[cfg(feature = "snapshot")]
use marspot::ui::components::context_menu_view::MenuRender;
#[cfg(feature = "snapshot")]
use marspot::ui::components::MenuItem;

mod bench_chrome;
mod bench_chrome_change;

/// Headless modes (snapshot / bench parse / bench render) use a fixed
/// terminal grid so numbers are reproducible across runs.
const GRID_COLS: u16 = 80;
const GRID_ROWS: u16 = 24;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = parse_named_arg(&args, "--snapshot") {
        let panel = parse_named_arg(&args, "--panel");
        #[cfg(feature = "snapshot")]
        {
            run_snapshot(&path, panel.as_deref());
            return;
        }
        #[cfg(not(feature = "snapshot"))]
        {
            let _ = (path, panel);
            eprintln!(
                "--snapshot needs a build with the tool compiled in:\n  \
                 cargo build --features snapshot --bin marspot"
            );
            std::process::exit(2);
        }
    }
    if let Some(spec) = parse_named_arg(&args, "--bench") {
        run_bench(&spec);
        return;
    }
    eprintln!(
        "usage: marspot --bench <mode>:<arg>\n       \
         marspot --snapshot <out.png> [--panel <name>]   (needs the `snapshot` feature)"
    );
    std::process::exit(2);
}

fn parse_named_arg(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter().skip(1);
    let prefix = format!("{}=", name);
    while let Some(a) = iter.next() {
        if a == name {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(prefix.as_str()) {
            return Some(rest.to_string());
        }
    }
    None
}

#[cfg(feature = "snapshot")]
/// Headless render: one frame into an offscreen texture, out as a PNG.
///
/// `--snapshot <path>` draws a demo terminal frame; `--panel settings`
/// opens a panel over it first.
///
/// It answers *how does this panel actually look?* without a user
/// screenshot or a window raised over whatever they are doing.
fn run_snapshot(path: &str, panel: Option<&str>) {
    use marspot::render_metal::{make_readback_texture, MetalRenderer, WindowRender};
    use marspot_term::layout::Layout;

    // `MARSPOT_SHOT_SCALE` renders as a display of that density would.
    // Set before the renderer: the font cache bakes the scale into the
    // terminal cell when it is built.
    if let Some(s) = std::env::var("MARSPOT_SHOT_SCALE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        marspot::ui::set_chrome_scale(s);
    }

    let (phys_w, phys_h): (u32, u32) = (1600, 1100);
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("--snapshot: no headless Metal renderer: {e}");
            std::process::exit(2);
        }
    };
    let target = match make_readback_texture(renderer.device(), phys_w, phys_h) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("--snapshot: {e}");
            std::process::exit(2);
        }
    };

    let (cell_w, cell_h) = renderer.cell_dims();
    let cols = ((phys_w as f64 / cell_w) as u16).max(8);
    let rows = ((phys_h as f64 / cell_h) as u16).max(4);
    let mut terminal = Terminal::new(cols, rows);
    // Something to sit behind the panel, so the backdrop dim is
    // visible for what it is.
    for i in 0..rows.saturating_sub(1) as usize {
        terminal.feed(
            format!(
                "\x1b[3{}m{:>3} \x1b[0mthe quick brown fox jumps over the lazy dog                   ①②③ 汉字 ABC 0123456789\r\n",
                1 + (i % 6),
                i
            )
            .as_bytes(),
        );
    }

    let layout = Layout::build(
        phys_w as f64, phys_h as f64, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h,
    );
    // The search bar is painted *inside* a pane, not as a modal, so it
    // rides on the view rather than on a renderer slot.
    let search = (panel == Some("search")).then(|| {
        use marspot_term::render::{SearchHitView, SearchOverlayView};
        let hit = |snippet: &str, focused: bool| SearchHitView {
            snippet: snippet.to_string(),
            is_focused: focused,
        };
        SearchOverlayView {
            query: "reclaim".to_string(),
            query_cursor: 7,
            case_sensitive: false,
            counter: Some((2, 17)),
            hits: vec![
                hit("  reclaim.enabled = false", false),
                hit("fn reclaim_op(uuid: &str, config_dir: Option<&str>) -> Option<PtyOp> {", true),
                hit("    // A pane whose session file has not appeared yet", false),
                hit("hibernate.start … idle=1800s reclaim=on", false),
            ],
        }
    });
    let view = SessionView {
        recede: 0,
        scrim: 0.0,
        grid: terminal.grid(),
        view_offset: 0,
        cursor_visible: true,
        focused: true,
        title: "snapshot",
        selection: None,
        ime_preedit: "",
        dormant: false,
        right_badge: "",
        agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: search,
        history: Default::default(),
        seq: 0,
    };

    match panel {
        None => {}
        Some("settings") => {
            let settings = marspot::settings::get();
            let rect = {
                let mut measure = marspot::ui::components::controls::font_measure(renderer.font_mut());
                marspot::ui::components::settings_modal::panel_rect(
                    phys_w as f64, phys_h as f64, &settings, &mut measure, layout.top_inset,
                )
            };
            renderer.set_settings_panel(Some(marspot::ui::components::settings_view::SettingsRender::new(
                rect,
                0.0,
                (*settings).clone(),
                &marspot::settings::path().display().to_string(),
            )));
        }
        Some("cc") => {
            let rect = marspot::ui::components::cc_usage_modal::panel_rect(
                2, 4, phys_w as f64, phys_h as f64, cell_w, cell_h, layout.top_inset,
            );
            renderer.set_cc_usage(Some(demo_cc_usage(rect)));
        }
        Some("search") => {} // rides on the view above
        Some("layout") => {
            // Six columns and long project names — the shape that
            // overflowed its own modal (2026-08-11).
            renderer.set_layout_modal(Some(marspot::ui::components::layout_view::LayoutModalRender {
                cols: std::env::var("MARSPOT_SHOT_COLS")
                    .ok().and_then(|v| v.parse().ok()).unwrap_or(6),
                rows: std::env::var("MARSPOT_SHOT_ROWS")
                    .ok().and_then(|v| v.parse().ok()).unwrap_or(2),
                slot_titles: {
                    let names = [
                        "marspot", "torajs", "spg", "smix", "devops", "insight",
                        "mailrs", "sentori", "lab36-continus", "goliajp", "kevy",
                        "lab38-golialab",
                    ];
                    let n = std::env::var("MARSPOT_SHOT_COLS")
                        .ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(6)
                        * std::env::var("MARSPOT_SHOT_ROWS")
                            .ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2);
                    (0..n).map(|i| names[i % names.len()].to_string()).collect()
                },
                ..Default::default()
            }));
        }
        Some("menu") => {
            let item = |label: &str, hint: &str, enabled: bool| {
                let row = MenuItem::entry(label, 0).with_shortcut(hint);
                if enabled { row } else { row.disabled() }
            };
            let mut menu = MenuRender::new(
                (420.0, 300.0),
                layout.top_inset,
                vec![
                    item("Split right", "⌘D", true),
                    item("Split down", "⇧⌘D", true),
                    MenuItem::divider(),
                    item("Never reclaim this pane", "", true),
                    item("Copy pane title", "", true),
                    MenuItem::divider(),
                    item("Close pane", "⌘W", true),
                    item("Close other panes", "", false),
                ],
            );
            menu.surface.borrow_mut().lay_out(renderer.font_mut(), &menu, (phys_w as f64, phys_h as f64));
            menu.pointer = menu.surface.borrow().row_centre(3);
            renderer.set_context_menu(Some(menu));
        }
        Some("process") => {
            let (cell_w, cell_h) = renderer.cell_dims();
            renderer.set_process_panel(Some(demo_process_panel(
                phys_w as f64, phys_h as f64, layout.top_inset, cell_w, cell_h,
            )));
        }
        Some(other) => {
            eprintln!(
                "--snapshot: unknown panel {other:?} (known: settings, cc, process, layout, menu, search)"
            );
            std::process::exit(2);
        }
    }

    let mut wr = WindowRender::new();
    let views = std::slice::from_ref(&view);
    // Twice: the first frame populates the glyph atlas, and glyphs
    // rasterised mid-frame land in the *next* one.  A one-frame
    // snapshot of a cold atlas is a picture of missing text.
    renderer.render_layout_to_texture(&mut wr, &target, &layout, views, &[], 0);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, views, &[], 0);

    let bgra = match renderer.read_target(&target) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("--snapshot: readback failed: {e}");
            std::process::exit(2);
        }
    };
    let png = match marspot::png::encode_bgra(phys_w, phys_h, &bgra) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("--snapshot: encode failed: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = std::fs::write(path, &png) {
        eprintln!("--snapshot: write {path}: {e}");
        std::process::exit(2);
    }
    println!("{path} ({phys_w}x{phys_h}, {} bytes)", png.len());
}


#[cfg(feature = "snapshot")]
/// Stand-in data for `--snapshot --panel cc`.
///
/// Fixed values, never the live feed: a snapshot whose content moves
/// cannot be compared against the one before it, and comparing is the
/// whole point — the panels are supposed to look like one program.
fn demo_cc_usage(rect: marspot_term::layout::Rect) -> marspot::ui::components::cc_usage_modal::CcUsageRender {
    use marspot::ui::components::cc_usage_modal::{CcUsageAccountRender, CcUsageRender};
    // A fixed instant, so the timeline lands in the same place every
    // run: 2026-01-02 03:00:00 UTC.
    let now = 1_767_322_800i64;
    let win = |label: &str, util: f32, reset: i64, hm: &str, span: i64| {
        marspot::ui::components::cc_usage_modal::CcUsageWindowRender {
            label: label.to_string(),
            util: Some(util),
            reset_unix: Some(reset),
            span_secs: span,
            reset_hm: hm.to_string(),
        }
    };
    let account = |name: &str, email: &str, status: &str, sev: u8, a: f32, b: f32| {
        CcUsageAccountRender {
            name: name.to_string(),
            email: email.to_string(),
            status_label: status.to_string(),
            status_severity: sev,
            reset_label: "reset: 01/02 06:00".to_string(),
            windows: vec![
                win("5h", a, now + 3 * 3600, "06:00", 5 * 3600),
                win("7d", b, now + 4 * 24 * 3600, "01/06", 7 * 86_400),
                win("opus", a, now + 4 * 24 * 3600, "01/06", 7 * 86_400),
                win("sonnet", b * 0.5, now + 4 * 24 * 3600, "01/06", 7 * 86_400),
            ],
        }
    };
    CcUsageRender {
        rect,
        updated_label: "updated 01/02 03:00".to_string(),
        accounts: vec![
            account("primary", "one@example.com", "ok", 0, 0.42, 0.61),
            account("spare", "two@example.com", "near limit", 1, 0.88, 0.35),
        ],
        clock: marspot::ui::components::cc_usage_modal::Clock::fixed(now, 0),
        pointer: None,
        surface: Default::default(),
    }
}

#[cfg(feature = "snapshot")]
/// Stand-in data for `--snapshot --panel process`.
fn demo_process_panel(
    w_phys: f64,
    h_phys: f64,
    top_inset: f64,
    cell_w: f64,
    cell_h: f64,
) -> marspot::ui::components::process_panel::ProcessPanelRender {
    use marspot::ui::components::process_panel::{
        self as pp, ProcessPanelPaneRow, ProcessPanelRender, ProcessPanelRow,
    };
    let pane = |name: &str, sid: u64, n: u32, cpu: f32, rss: u64, busy: &str| {
        ProcessPanelPaneRow {
            name: name.to_string(),
            sid,
            n_pids: n,
            cpu_pct: cpu,
            rss_kb: rss,
            busy: busy.to_string(),
        }
    };
    let row = |depth: u8, pid: i32, comm: &str, cpu: f32, rss: u64| {
        ProcessPanelRow { depth, pid, comm: comm.to_string(), cpu_pct: cpu, rss_kb: rss }
    };
    let pane_rows = vec![
        pane("torajs", 102, 9, 34.5, 512_000, "cargo test"),
        pane("marspot", 101, 4, 1.2, 86_000, ""),
        pane("kevy", 103, 2, 0.0, 12_400, ""),
    ];
    let rows = vec![
        row(0, 4211, "zsh", 0.0, 3_200),
        row(1, 4288, "cargo", 2.1, 48_000),
        row(2, 4301, "rustc", 32.4, 460_000),
        row(2, 4302, "rustc", 0.0, 900),
    ];
    let frame = pp::panel_rect(w_phys, h_phys, cell_h, top_inset, (0.0, 0.0));
    ProcessPanelRender {
        layout: pp::layout(frame, top_inset, pane_rows.len(), rows.len(), 0.0, cell_w, cell_h),
        pane_rows,
        selected_pane: 0,
        detail: "torajs · sid 102 · shell pid 4211".to_string(),
        rows,
        pointer: None,
        surface: Default::default(),
    }
}

/// Read this process's resident set size in KiB via Mach
/// `task_info(MACH_TASK_BASIC_INFO)`.  ~10 µs per call on Apple
/// Silicon.  Returns 0 if the syscall fails.
// libc 0.2 deprecates its mach bindings (both `mach_task_self()` the
// function and `mach_task_self_` the static) in favour of the `mach2`
// crate.  Adding a new FFI dep just to silence the warning would
// violate the build-over-depend principle for a static that is
// still fully functional — accept the deprecation locally instead.
#[allow(deprecated)]
fn read_self_rss_kib() -> usize {
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
            / std::mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        let result = libc::task_info(
            libc::mach_task_self_,
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return 0;
        }
        (info.resident_size / 1024) as usize
    }
}

/// Headless benchmark dispatcher.  Spec is `<mode>:<arg>`.
///
/// Modes:
///   parse:<path>   feed bytes from `path` through Terminal::feed and
///                  report wall-clock throughput (bytes/sec)
///   render:<n>     run `n` full-frame renders against a synthetic
///                  worst-case grid (full coloured cells), report
///                  per-frame p50/p95/p99 nanoseconds
///   scroll:<path>[:<start>[:<step>]]
///                  feed `path` to populate scrollback, then walk
///                  view_offset from `start` (default 5000) toward 0
///                  in `step`-line decrements (default 3 — one wheel
///                  detent) and time each viewport repaint via
///                  `Grid::cell_at_view`.  Reports per-tick p50/p95/p99
///                  nanoseconds.  Runs against the Memory variant
///                  (the bench's default Terminal without
///                  `MARSPOT_SESSION_ID`).
///
/// All modes write a single line of JSON to stdout so harness scripts
/// can grep / parse without depending on prose formatting.
fn run_bench(spec: &str) {
    let (mode, arg) = match spec.split_once(':') {
        Some(p) => p,
        None => {
            eprintln!(
                "--bench expects <mode>:<arg>, e.g. parse:/tmp/cat-ascii.bin or render:1000"
            );
            std::process::exit(2);
        }
    };
    match mode {
        "parse" => bench_parse(arg),
        // Two cuts through the LIVE pipeline, so the gap between
        // `parse` (no PTY, no window) and what a real terminal
        // delivers can be attributed instead of guessed:
        //   pty-drain  PTY + reader thread only — bytes read and
        //              dropped.  The ceiling any terminal on this
        //              machine can reach through a pty.
        //   pty-feed   the above plus Terminal::feed — everything a
        //              terminal does except draw.
        // The live app (bin/measure-l3.sh, and the app rows of
        // `bin/bench.sh --full`) adds L3, the core and Metal on top.
        #[cfg(feature = "bench-pty")]
        "pty-raw" => bench_pty_raw(arg),
        #[cfg(feature = "bench-pty")]
        "pty-drain" => bench_pty(arg, /* feed */ false),
        #[cfg(feature = "bench-pty")]
        "pty-feed" => bench_pty(arg, /* feed */ true),
        // `render` and `metal-render` are now aliases — the AppKit
        // CGImage renderer was removed, Metal is the only live path.
        "render" | "metal-render" => bench_metal_render(arg),
        "chrome-render" => bench_chrome::run(arg),
        "chrome-change" => bench_chrome_change::run(arg),
        "scroll" => bench_scroll(arg, /* cold */ false),
        "scroll-cold" => bench_scroll(arg, /* cold */ true),
        "scrollaccess" => bench_scrollaccess(arg),
        "glyphraster" => bench_glyphraster(arg),
        "first-frame" => bench_first_frame(arg),
        other => {
            eprintln!("unknown bench mode: {other}");
            std::process::exit(2);
        }
    }
}

/// `--bench scrollaccess:<lines>` — scrollback ACCESS latency at depth
/// (perf-attack D re-scope).  Builds a `<lines>`-deep disk-backed
/// scrollback, then measures the **cold** (post-MADV_DONTNEED) latency to
/// read one viewport-worth of cells at depths spanning orders of magnitude
/// (0, 1k, 10k, 100k, 1M, near-max).  This is the structural edge over
/// Terminal.app / iTerm2 (bounded history — they can't access deep at
/// all): marspot's anon-mmap ring + line index makes a deep read an O(1)
/// page-fault at the target line regardless of depth.  Reads via
/// `scrollback_cell(idx: usize, …)` — the O(1) ring primitive — NOT
/// `cell_at_view`, whose `view_offset: u16` caps the *viewport scroll* at
/// 65 535 lines (a UI-scroll limit; the data underneath is addressable to
/// the full `scrollback_len()`).  Reports per-depth cold ns + resident
/// RSS so the gate can assert flatness (max/min small) + bounded memory.
fn bench_scrollaccess(arg: &str) {
    let target_lines: usize = arg.parse().unwrap_or(2_000_000).max(GRID_ROWS as usize + 1);

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    // Feed target_lines short (~64 B) lines in batches — avoids a giant
    // single Vec while still exercising wrap / index logic per line.
    let batch_lines = 20_000usize;
    let mut buf: Vec<u8> = Vec::with_capacity(batch_lines * 70);
    let mut written = 0usize;
    while written < target_lines {
        let n = batch_lines.min(target_lines - written);
        buf.clear();
        for i in 0..n {
            let idx = written + i;
            buf.extend_from_slice(
                format!("line {idx:08}: lorem ipsum dolor sit amet consectetur adipi\r\n").as_bytes(),
            );
        }
        terminal.feed(&buf);
        written += n;
    }

    let sb_len = terminal.grid().scrollback_len();
    let max_idx = sb_len.saturating_sub(1);
    // Depths (lines back from the newest scrollback line) to probe.
    let mut depths: Vec<usize> = [0usize, 1_000, 10_000, 100_000, 1_000_000, max_idx]
        .iter()
        .copied()
        .filter(|&d| d <= max_idx)
        .collect();
    depths.sort_unstable();
    depths.dedup();

    let rss_kib = read_self_rss_kib();

    let cols = GRID_COLS;
    let rows = GRID_ROWS as usize;
    let mut sink: u64 = 0;
    let mut parts: Vec<String> = Vec::new();
    for &d in &depths {
        // Newest scrollback line is index sb_len-1; depth d sits d lines
        // older. Read a viewport-worth of lines going older from there.
        let top = max_idx.saturating_sub(d);
        // F2 — the historical Disk variant let bench drop ring pages
        // to simulate a cold cache; with Memory as the bench default
        // there is no page cache to drop.  Real cold-cache behaviour
        // lives in production File scrollback's pread fallback path
        // and is bench-validated via mini live runs instead.
        let t0 = std::time::Instant::now();
        for r in 0..rows {
            let idx = top.saturating_sub(r);
            for c in 0..cols {
                if let Some(cell) = terminal.grid().scrollback_cell(idx, c) {
                    sink = sink.wrapping_add(cell.ch as u64);
                }
            }
        }
        let ns = t0.elapsed().as_nanos() as u64;
        parts.push(format!(r#"{{"depth":{d},"cold_ns":{ns}}}"#));
    }
    std::hint::black_box(sink);

    println!(
        r#"{{"mode":"scrollaccess","target_lines":{},"sb_len":{},"rss_kib":{},"depths":[{}]}}"#,
        target_lines,
        sb_len,
        rss_kib,
        parts.join(",")
    );
}

/// `--bench glyphraster:<N>` — headless glyph-rasterisation throughput
/// (perf-attack B3/B4 root-cause confirmation).  For each script class
/// (ascii / cjk / emoji) it rasterises up to N DISTINCT glyphs through
/// the real cache-miss path on a fresh (cold-atlas) renderer and reports
/// ns/glyph + glyphs/s.  This is the cost the core's render thread pays
/// when a CJK/emoji firehose first shows each glyph — the hypothesised
/// hot spot is the per-glyph `CGBitmapContextCreate` + buffer alloc +
/// context-property setup in `glyph_atlas::rasterise_glyph`.  No GPU
/// draw, no parse: the number isolates rasterisation alone.  Headless
/// (Metal device only needed for the atlas texture), so it runs in the
/// gate.  NOTE: this measures the *render* half; in the L3 architecture
/// glyph render is decoupled from cat throughput (the core reads the
/// latest shm snapshot and never back-pressures L3 unless pokes pile
/// up), so a slow number here shows as frame latency under churn, not as
/// lower cat MiB/s.
fn bench_glyphraster(arg: &str) {
    let n: usize = arg.parse().unwrap_or(1000).max(1);

    // Distinct chars per class. ascii printable is only 94 wide; cjk
    // walks the Unified Ideographs block; emoji chains the common emoji
    // blocks (unassigned codepoints resolve to glyph 0 and are skipped
    // by resolve_cell_glyph — a cheap lookup, negligible over N).
    let ascii: Vec<char> = (0x21u32..=0x7E).filter_map(char::from_u32).collect();
    let cjk: Vec<char> = (0x4E00u32..)
        .filter_map(char::from_u32)
        .take(n)
        .collect();
    let emoji: Vec<char> = (0x1F300u32..=0x1FAFF)
        .filter_map(char::from_u32)
        .take(n)
        .collect();

    let classes: [(&str, &[char]); 3] = [
        ("ascii", &ascii),
        ("cjk", &cjk),
        ("emoji", &emoji),
    ];

    let mut parts: Vec<String> = Vec::new();
    for (name, chars) in classes {
        // Fresh renderer per class → cold atlas, so every char is a miss.
        let mut renderer = MetalRenderer::new_headless().expect("headless metal renderer");
        let total_ns = renderer.bench_rasterize(chars);
        let count = chars.len();
        let ns_per = if count > 0 { total_ns / count as u64 } else { 0 };
        let per_sec = if total_ns > 0 {
            (count as f64) * 1e9 / (total_ns as f64)
        } else {
            0.0
        };
        parts.push(format!(
            r#""{}":{{"glyphs":{},"total_ns":{},"ns_per_glyph":{},"glyphs_per_sec":{:.0}}}"#,
            name, count, total_ns, ns_per, per_sec
        ));
    }
    println!(r#"{{"mode":"glyphraster",{}}}"#, parts.join(","));
}

fn bench_parse(spec: &str) {
    // `parse:<path>[:<repeat>[:<chunk_bytes>]]`
    //
    // `repeat` — a single pass over an 8 MB scenario is ~100 ms, too
    // short a window for a sampling profiler to say which work
    // dominates.  Feeds the same bytes N times, each into a fresh
    // terminal.  Timing covers only `feed`, so terminal construction
    // never lands in the number.
    //
    // `chunk_bytes` — feed in slices of this size instead of one call.
    // The live pipeline never hands the parser a whole corpus: the
    // reader thread delivers READ_BUF (64 KiB) at a time and `pump`
    // calls `feed` once per chunk.  That matters because the batch
    // lanes are scans over a contiguous slice — a run that would have
    // spanned a chunk boundary gets split, and the tail of every chunk
    // falls back to the scalar path.  Measuring one-shot feed and
    // calling it "the parser's speed" overstates what the live path
    // can reach.  0 (default) = one call, i.e. the old behaviour.
    let (path, repeat, chunk) = {
        let mut rest = spec;
        let mut nums: Vec<usize> = Vec::new();
        while nums.len() < 2 {
            match rest.rsplit_once(':') {
                Some((head, tail)) => match tail.parse::<usize>() {
                    Ok(n) => {
                        nums.push(n);
                        rest = head;
                    }
                    Err(_) => break,
                },
                None => break,
            }
        }
        nums.reverse();
        let repeat = nums.first().copied().filter(|&n| n > 0).unwrap_or(1) as u32;
        let chunk = nums.get(1).copied().unwrap_or(0);
        (rest, repeat, chunk)
    };
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("bench: read {path}: {e}");
        std::process::exit(2);
    });
    // Use the same grid dimensions as a typical marspot window (auto-fit
    // 122×39 on the user's default 960×600 layout) so the parser path
    // exercises wrap / scroll the way it does in real use.
    let mut elapsed_ns: u64 = 0;
    for _ in 0..repeat {
        let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
        let t0 = std::time::Instant::now();
        if chunk == 0 {
            terminal.feed(&bytes);
        } else {
            for slice in bytes.chunks(chunk) {
                terminal.feed(slice);
            }
        }
        elapsed_ns += t0.elapsed().as_nanos() as u64;
    }
    let total = bytes.len() as u128 * repeat as u128;
    let bytes_per_sec = if elapsed_ns > 0 {
        (total * 1_000_000_000 / elapsed_ns as u128) as u64
    } else {
        0
    };
    println!(
        r#"{{"mode":"parse","path":"{}","bytes":{},"repeat":{},"chunk":{},"elapsed_ns":{},"bytes_per_sec":{}}}"#,
        path,
        bytes.len(),
        repeat,
        chunk,
        elapsed_ns,
        bytes_per_sec
    );
}

/// `--bench pty-raw:<path>[:<repeat>]` — the floor of the bytes path.
///
/// `cat <path>` under a pty, drained by a bare blocking `read(2)` loop
/// on the master: no reader thread, no channel, no `Vec` per batch, no
/// probe polls.  Everything left in the number belongs to the child's
/// `write`s, the kernel tty layer (including the ONLCR expansion that
/// makes the reader see more bytes than are on disk) and the read
/// syscalls themselves.
///
/// It exists to keep the decomposition honest.  `pty-drain` costs
/// 5.7 ms/MB on cjk while ghostty's WHOLE pipeline costs 6.9 — which
/// either means our gather architecture is nearly all of the gap, or
/// means most of that 5.7 is kernel work both terminals pay.  The
/// difference decides which subsystem is worth attacking, and no
/// amount of reading either codebase settles it.
#[cfg(feature = "bench-pty")]
fn bench_pty_raw(spec: &str) {
    let (path, repeat) = match spec.rsplit_once(':') {
        Some((p, n)) => match n.parse::<u32>() {
            Ok(n) if n > 0 => (p, n),
            _ => (spec, 1),
        },
        None => (spec, 1),
    };
    let bytes_on_disk = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("bench: stat {path}: {e}");
            std::process::exit(2);
        }
    };
    let abs = match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("bench: resolve {path}: {e}");
            std::process::exit(2);
        }
    };
    let mut elapsed_ns: u64 = 0;
    let mut total_read: u64 = 0;
    let mut reads: u64 = 0;
    for _ in 0..repeat {
        let pty = match marspot_term::pty::Pty::spawn(marspot_term::pty::PtyConfig {
            program: "/bin/cat".into(),
            args: vec![abs.clone()],
            size: marspot_term::pty::TerminalSize {
                cols: GRID_COLS,
                rows: GRID_ROWS,
                pixel_width: 0,
                pixel_height: 0,
            },
            argv0: None,
            cwd: None,
            env_remove_prefixes: Vec::new(),
            env_set: Vec::new(),
        }) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bench: spawn cat under pty: {e}");
                std::process::exit(2);
            }
        };
        let fd = pty.raw_master();
        let mut buf = vec![0u8; 64 * 1024];
        let round_start = total_read;
        let t0 = std::time::Instant::now();
        while total_read - round_start < bytes_on_disk {
            let n = unsafe {
                libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                break;
            }
            reads += 1;
            total_read += n as u64;
        }
        elapsed_ns += t0.elapsed().as_nanos() as u64;
        drop(pty);
    }
    let bytes_per_sec = if elapsed_ns > 0 {
        (total_read as u128 * 1_000_000_000 / elapsed_ns as u128) as u64
    } else {
        0
    };
    println!(
        r#"{{"mode":"pty-raw","path":"{}","bytes":{},"repeat":{},"reads":{},"bytes_per_read":{:.1},"elapsed_ns":{},"bytes_per_sec":{}}}"#,
        path,
        total_read,
        repeat,
        reads,
        if reads > 0 { total_read as f64 / reads as f64 } else { 0.0 },
        elapsed_ns,
        bytes_per_sec
    );
}

/// `--bench pty-drain:<path>[:<repeat>]` / `pty-feed:<path>[:<repeat>]`
///
/// Runs `cat <path>` under a real PTY and drains it the way the live
/// pipeline does — reader thread, 64 KiB reads, channel — either
/// dropping the bytes (`pty-drain`) or feeding them to a terminal
/// (`pty-feed`).  No window, no renderer, no event loop.
///
/// Why both: `--bench parse` says how fast the parser is with the
/// corpus already in memory, and `bin/measure-l3.sh` says what a
/// user gets.  Between those two numbers sit the pty, the reader thread,
/// the parser, the event loop and the GPU, and a single ratio cannot
/// say which of them owns the difference.  These two cuts split it:
/// drain is the pty's own ceiling, feed adds the parser, and whatever
/// is left over in the live number belongs to the window.
#[cfg(feature = "bench-pty")]
fn bench_pty(spec: &str, feed: bool) {
    let (path, repeat) = match spec.rsplit_once(':') {
        Some((p, n)) => match n.parse::<u32>() {
            Ok(n) if n > 0 => (p, n),
            _ => (spec, 1),
        },
        None => (spec, 1),
    };
    let bytes_on_disk = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("bench: stat {path}: {e}");
            std::process::exit(2);
        }
    };
    // The child is spawned with cwd=$HOME (Session's rule, so a user's
    // shell opens somewhere sensible), so a relative path here would
    // reach `cat` as a path relative to the wrong directory.  It fails
    // in the most confusing way available: `cat` writes its error to
    // the pty, the drain loop counts those 61 bytes as corpus, and the
    // bench reports a timeout instead of "no such file".
    let abs = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bench: resolve {path}: {e}");
            std::process::exit(2);
        }
    };
    let abs = abs.to_string_lossy().into_owned();
    let mut elapsed_ns: u64 = 0;
    let mut total_read: u64 = 0;
    for _ in 0..repeat {
        // Block on the reader's wake instead of spinning.  A spin loop
        // here competes with the reader thread and `cat` for cores and
        // then reports the result as "the pty's throughput" — the
        // measurement would be of the measurement.  `wake` fires from
        // the reader thread whenever bytes land.
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let mut session = match marspot_term::session::Session::spawn_with(
            "/bin/cat",
            &[abs.as_str()],
            GRID_COLS,
            GRID_ROWS,
            move || {
                let _ = wake_tx.send(());
            },
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bench: spawn cat under pty: {e}");
                std::process::exit(2);
            }
        };
        let t0 = std::time::Instant::now();
        // Spin rather than sleep: a sleep would measure the sleep.
        // The child is `cat`, so this loop is never long-lived.
        //
        // Termination is by byte count, NOT by `is_exited`: the pty
        // keeps the slave fd open for its own lifetime, so the master
        // never sees EOF just because the child left, and waiting for
        // that flag hangs until the timeout.  Counting up to the file
        // size is sound in the other direction — the line discipline's
        // ONLCR turns every \n into \r\n, so the reader always sees at
        // least as many bytes as are on disk.
        let round_start = total_read;
        loop {
            if feed {
                total_read += session.pump() as u64;
            } else {
                total_read += session.drain_raw().len() as u64;
            }
            if total_read - round_start >= bytes_on_disk {
                break;
            }
            if t0.elapsed().as_secs() > 120 {
                eprintln!("bench: pty drain timed out");
                break;
            }
            // Drain first, then park until the reader says there is
            // more.  The timeout only exists so a lost wake cannot
            // hang the bench.
            let _ = wake_rx.recv_timeout(std::time::Duration::from_millis(50));
        }
        elapsed_ns += t0.elapsed().as_nanos() as u64;
    }
    let bytes_per_sec = if elapsed_ns > 0 {
        (total_read as u128 * 1_000_000_000 / elapsed_ns as u128) as u64
    } else {
        0
    };
    // How the bytes arrived, not just how fast: `bytes_per_read` is
    // the kernel tty queue's real handout size, and `bytes_per_batch`
    // is how much one wakeup gathered.  If the second is barely larger
    // than the first, the reader is sleeping once per kilobyte.
    use std::sync::atomic::Ordering as AtomicOrdering;
    let reads = marspot_term::session::PTY_READ_CALLS.load(AtomicOrdering::Relaxed);
    let batches = marspot_term::session::PTY_GATHER_BATCHES.load(AtomicOrdering::Relaxed);
    let read_bytes = marspot_term::session::PTY_READ_BYTES.load(AtomicOrdering::Relaxed);
    println!(
        r#"{{"reads":{},"batches":{},"read_bytes":{},"bytes_per_read":{:.1},"bytes_per_batch":{:.1}}}"#,
        reads,
        batches,
        read_bytes,
        if reads > 0 { read_bytes as f64 / reads as f64 } else { 0.0 },
        if batches > 0 { read_bytes as f64 / batches as f64 } else { 0.0 },
    );
    println!(
        r#"{{"mode":"{}","path":"{}","bytes":{},"repeat":{},"elapsed_ns":{},"bytes_per_sec":{}}}"#,
        if feed { "pty-feed" } else { "pty-drain" },
        path,
        total_read,
        repeat,
        elapsed_ns,
        bytes_per_sec
    );
}

/// `--bench first-frame:<panes>[:<charset>]` — what the **first**
/// frame after a core boot costs.
///
/// Every other render bench warms up first: `bench_metal_render`
/// renders five throwaway frames precisely so the glyph atlas and the
/// GPU caches are hot before the stopwatch starts.  That is the right
/// call for a steady-state gate, and it is why no measurement in this
/// repo has ever covered the frame that actually hurts — the one a
/// freshly spawned core draws into a newly attached IOSurface with an
/// atlas containing nothing at all.  A core is killed and respawned on
/// every crash, hang verdict, silent update and window attach, so this
/// frame is not an edge case; it is the recovery path.
///
/// Reports the cold frame and the next (warm) one side by side, each
/// split into CPU instance-building / command encoding / GPU wait,
/// with the number of glyphs rasterised.  `charset` picks how much
/// glyph diversity the panes hold: `ascii` (~95 distinct), `cjk`
/// (thousands), `mixed` (default) — the spread between them is the
/// answer to "how much of this is the cold atlas".
fn bench_first_frame(arg: &str) {
    let mut parts = arg.split(':');
    let panes: usize = parts.next().unwrap_or("13").parse().unwrap_or_else(|_| {
        eprintln!("bench: first-frame needs <panes>[:<charset>]");
        std::process::exit(2);
    });
    let charset = parts.next().unwrap_or("mixed");
    // The dims from the incident this bench exists for: a 4K window,
    // 13 panes on a 6×2 grid + 1, 86×63 cells each.
    let phys_w: u32 = 3840;
    let phys_h: u32 = 2130;
    let grid_cols = 6usize;
    let grid_rows = panes.div_ceil(grid_cols).max(1);

    // Fill every pane with text of the requested diversity.  CJK
    // sweeps a contiguous CJK block so each cell is a glyph the atlas
    // has never seen; ASCII repeats the printable range.
    // CJK Unified Ideographs, and nothing past it.  The first version
    // of this bench walked `0x4E00 + pane * 4096` unbounded, which ran
    // off the end of the block into musical symbols and emoji by pane
    // 4 — and emoji rasterise at ~630 µs against CJK's ~11 µs, so the
    // "cjk" row was reporting a per-glyph cost 15× the real one.  A
    // measurement instrument that silently changes what it measures
    // looks exactly like a finding.
    const CJK_LO: u32 = 0x4E00;
    const CJK_HI: u32 = 0x9FFF;
    let cjk_span = CJK_HI - CJK_LO + 1;
    let mut terminals: Vec<Terminal> = Vec::with_capacity(panes);
    for p in 0..panes {
        let mut t = Terminal::new(86, 63);
        let mut payload: Vec<u8> = Vec::with_capacity(64 * 1024);
        // Panes start at different offsets so they don't all draw the
        // same glyphs, but every one stays inside the block.
        let mut cjk_i: u32 = (p as u32).wrapping_mul(4096) % cjk_span;
        for r in 0..63u16 {
            for c in 0..86u16 {
                let want_cjk = match charset {
                    "ascii" => false,
                    "cjk" => true,
                    _ => (r as usize + c as usize).is_multiple_of(3),
                };
                if want_cjk {
                    let ch = char::from_u32(CJK_LO + cjk_i).unwrap_or('中');
                    cjk_i = (cjk_i + 1) % cjk_span;
                    let mut buf = [0u8; 4];
                    payload.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                } else {
                    payload.push(((c % 95) as u8) + 32);
                }
            }
            if r + 1 < 63 {
                payload.extend_from_slice(b"\r\n");
            }
        }
        t.feed(&payload);
        terminals.push(t);
    }

    let mut renderer = MetalRenderer::new_headless().expect("headless metal renderer");
    let target = make_target_texture(renderer.device(), phys_w, phys_h)
        .expect("render-target texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(
        phys_w as f64, phys_h as f64, 0.0, 0.0, 0.0,
        grid_cols, grid_rows, cell_w, cell_h,
    );
    let views: Vec<SessionView> = terminals
        .iter()
        .map(|t| SessionView {
            recede: 0,
            scrim: 0.0,
            grid: t.grid(),
            view_offset: 0,
            cursor_visible: true,
            focused: false,
            title: "",
            selection: None,
            ime_preedit: "",
            dormant: false,
            right_badge: "",
            agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(),
            seq: 0,
        })
        .collect();

    let mut wr = WindowRender::new();
    // No warm-up.  That is the entire point.
    let t0 = std::time::Instant::now();
    renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &[], 0);
    let cold_ns = t0.elapsed().as_nanos() as u64;
    let cold = renderer.last_render_split();
    let t1 = std::time::Instant::now();
    renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &[], 0);
    let warm_ns = t1.elapsed().as_nanos() as u64;
    let warm = renderer.last_render_split();

    println!(
        r#"{{"mode":"first-frame","panes":{},"charset":"{}","cols":86,"rows":63,"phys":[{},{}],"cold":{{"total_ms":{:.2},"build_ms":{:.2},"cmdbuf_ms":{:.2},"encode_ms":{:.2},"instbuf_ms":{:.2},"instbuf_mb":{:.2},"canvas_ms":{:.2},"gpu_ms":{:.2},"glyphs":{},"evict":{},"rebuild":{}}},"warm":{{"total_ms":{:.2},"build_ms":{:.2},"encode_ms":{:.2},"gpu_ms":{:.2},"glyphs":{}}}}}"#,
        panes, charset, phys_w, phys_h,
        cold_ns as f64 / 1e6,
        cold.build_us as f64 / 1e3,
        cold.cmdbuf_us as f64 / 1e3,
        cold.encode_us as f64 / 1e3,
        cold.instbuf_us as f64 / 1e3,
        cold.instbuf_bytes as f64 / 1048576.0,
        cold.canvas_us as f64 / 1e3,
        cold.gpu_wait_us as f64 / 1e3,
        cold.glyphs_rasterised,
        cold.evictions,
        cold.rebuilds,
        warm_ns as f64 / 1e6,
        warm.build_us as f64 / 1e3,
        warm.encode_us as f64 / 1e3,

        warm.gpu_wait_us as f64 / 1e3,
        warm.glyphs_rasterised,
    );
}

/// Metal counterpart to `bench_render`.  Same worst-case grid (every
/// cell coloured + non-blank, alternating ASCII so glyph runs break
/// frequently) and same 960×600 target dims, but routes through
/// `MetalRenderer::render_layout_to_texture` so the timing covers
/// `build_instances` + BG/FG-pass encoding + GPU execution
/// (waitUntilCompleted blocks until the frame is fully rendered).
///
/// Apples-to-apples vs `--bench render` modulo two intentional
/// differences:
///   * No CGImage create + setContents (Metal renders straight to
///     the target).  This is the architectural reason for using Metal.
///   * No CPU-side pixel readback (the texture is StorageModePrivate).
///     The AppKit path's `snapshot` does include the BGRA copy-out.
fn bench_metal_render(arg: &str) {
    // `MARSPOT_QOS=1` raises this thread to USER_INTERACTIVE before
    // the run, so the same binary produces both halves of the A/B.
    if std::env::var("MARSPOT_QOS").as_deref() == Ok("1")
        && !marspot::qos::raise_current_thread_to_user_interactive()
    {
        eprintln!("bench: QoS request refused");
    }
    let n: u32 = arg.parse().unwrap_or_else(|_| {
        eprintln!("bench: metal-render needs an integer iteration count");
        std::process::exit(2);
    });

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    let mut payload: Vec<u8> = Vec::with_capacity(64 * 1024);
    for r in 0..GRID_ROWS {
        for c in 0..GRID_COLS {
            let colour = 30 + ((r as u32 + c as u32) % 8) as u8;
            payload.extend_from_slice(format!("\x1b[{}m", colour).as_bytes());
            let ch = ((c % 95) as u8) + 32;
            payload.push(ch);
        }
        if r + 1 < GRID_ROWS {
            payload.extend_from_slice(b"\r\n");
        }
    }
    terminal.feed(&payload);

    let mut renderer = MetalRenderer::new_headless().expect("headless metal renderer");
    let phys_w: u32 = 960;
    let phys_h: u32 = 600;
    let target = make_target_texture(renderer.device(), phys_w, phys_h)
        .expect("render-target texture");

    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(
        phys_w as f64,
        phys_h as f64,
        0.0,
        0.0,
        0.0,
        1,
        1,
        cell_w,
        cell_h,
    );
    let view = SessionView {
        recede: 0,
        scrim: 0.0,
        grid: terminal.grid(),
        view_offset: 0,
        cursor_visible: true,
        focused: true,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        right_badge: "",
        agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq: 0,
    };
    let views = std::slice::from_ref(&view);

    // The bench harness is one window, so one of these.
    let mut wr = WindowRender::new();
    // Warm-up: 5 iters fill char_cache + atlas + GPU caches.
    for _ in 0..5 {
        renderer.render_layout_to_texture(&mut wr, &target, &layout, views, &[], 0);
    }

    let mut samples: Vec<u64> = Vec::with_capacity(n as usize);
    // Per-component samples alongside the total.  The gate only reads
    // `p99_ns`, but a frame that got slower is a question, not an
    // answer, and answering it used to mean shipping a new build with
    // new timers.  `gpu_exec` in particular is the GPU's own account
    // of the work against `gpu_wait`'s wall clock: when the two
    // diverge, the frame was queued or descheduled rather than heavy,
    // and that distinction decides what to fix.
    let mut build: Vec<u64> = Vec::with_capacity(n as usize);
    let mut encode: Vec<u64> = Vec::with_capacity(n as usize);
    let mut gpu_wait: Vec<u64> = Vec::with_capacity(n as usize);
    let mut gpu_exec: Vec<u64> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        renderer.render_layout_to_texture(&mut wr, &target, &layout, views, &[], 0);
        samples.push(t0.elapsed().as_nanos() as u64);
        let sp = renderer.last_render_split();
        build.push(sp.build_us);
        encode.push(sp.cmdbuf_us + sp.encode_us + sp.canvas_us);
        gpu_wait.push(sp.gpu_wait_us);
        gpu_exec.push(sp.gpu_exec_us);
    }
    samples.sort_unstable();
    let pct = |v: &mut Vec<u64>, q: f64| -> u64 {
        v.sort_unstable();
        let idx = ((v.len() as f64) * q) as usize;
        v[idx.min(v.len() - 1)]
    };
    let p = |q: f64| -> u64 {
        let idx = ((samples.len() as f64) * q) as usize;
        samples[idx.min(samples.len() - 1)]
    };
    println!(
        r#"{{"mode":"metal-render","iterations":{},"p50_ns":{},"p95_ns":{},"p99_ns":{},"min_ns":{},"max_ns":{},"build_p50_us":{},"build_p99_us":{},"encode_p50_us":{},"encode_p99_us":{},"gpu_wait_p50_us":{},"gpu_wait_p99_us":{},"gpu_exec_p50_us":{},"gpu_exec_p99_us":{}}}"#,
        n,
        p(0.50),
        p(0.95),
        p(0.99),
        samples[0],
        samples[samples.len() - 1],
        pct(&mut build, 0.50), pct(&mut build, 0.99),
        pct(&mut encode, 0.50), pct(&mut encode, 0.99),
        pct(&mut gpu_wait, 0.50), pct(&mut gpu_wait, 0.99),
        pct(&mut gpu_exec, 0.50), pct(&mut gpu_exec, 0.99),
    );
}

/// `--bench scroll:<path>[:<start>[:<step>]]` — measure viewport repaint
/// latency under simulated downward scrolling.  This is the read-path
/// gate for disk-backed scrollback: the user's 99 %-case is "scroll
/// slowly toward live", so per-tick `cell_at_view` walks must stay
/// well under one frame budget (~16 ms; the floor we enforce is much
/// tighter).
///
/// `scroll-cold` is the same harness with one extra step between
/// feed and walk: it asks the kernel to evict the disk-backed
/// scrollback's resident pages (`MADV_DONTNEED`) so the walk
/// measures cold-page page-fault cost — the realistic experience of
/// a user who returns to scrollback hours after the writes.  F2:
/// the bench now runs against the Memory variant only; `cold` is
/// accepted for CLI compatibility but is a no-op.
///
/// Lifecycle:
///   1. Construct `Terminal` (Memory variant — bench has no
///      `MARSPOT_SESSION_ID`).
///   2. Feed the scenario file to populate scrollback.
///   3. (cold) no-op on Memory; retained for CLI compatibility.
///   4. Starting at `view_offset = start` (clamped to scrollback len),
///      walk every cell in the viewport via `Grid::cell_at_view` and
///      time the walk.  Decrement `view_offset` by `step` and repeat
///      until live (offset 0).
///   5. Report p50/p95/p99 nanoseconds per repaint.
fn bench_scroll(arg: &str, cold: bool) {
    let parts: Vec<&str> = arg.split(':').collect();
    if parts.is_empty() || parts[0].is_empty() {
        eprintln!("bench: scroll expects <path>[:<start>[:<step>]]");
        std::process::exit(2);
    }
    let path = parts[0];
    let start: u16 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let step: u16 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    if step == 0 {
        eprintln!("bench: scroll step must be > 0");
        std::process::exit(2);
    }

    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("bench: read {path}: {e}");
        std::process::exit(2);
    });

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    terminal.feed(&bytes);
    // F2 — `cold` previously dropped Disk variant ring pages to
    // simulate page-faulting scrollback reads.  Memory variant has
    // no pageable backing; flag retained on the CLI surface so the
    // bench-scripts contract stays compatible but the body is a
    // no-op now.  Production cold-read perf is validated via mini
    // live runs against the File variant's pread fallback.
    let _ = cold;

    // Clamp start to the actual scrollback depth — for memory storage
    // (10 K-line ring) feeding 100 K lines leaves only the most recent
    // 10 K addressable; pretending to start past that just measures
    // the "default cell" return path.
    let sb_len = terminal.grid().scrollback_len() as u16;
    let actual_start = start.min(sb_len);

    // Number of ticks: floor(actual_start / step) + 1 (final tick at 0).
    let tick_count = (actual_start as usize / step as usize) + 1;
    let mut samples: Vec<u64> = Vec::with_capacity(tick_count);
    let mut sink: u64 = 0;

    let mut offset = actual_start;
    loop {
        let t0 = std::time::Instant::now();
        for r in 0..GRID_ROWS {
            for c in 0..GRID_COLS {
                let cell = terminal.grid().cell_at_view(offset, c, r);
                sink = sink.wrapping_add(cell.ch as u64);
            }
        }
        samples.push(t0.elapsed().as_nanos() as u64);
        if offset == 0 {
            break;
        }
        offset = offset.saturating_sub(step);
    }
    // Black-hole the read sum so the optimiser can't elide the cell walk.
    std::hint::black_box(sink);

    samples.sort_unstable();
    let n = samples.len();
    let p = |q: f64| -> u64 {
        let idx = ((n as f64 - 1.0) * q).round() as usize;
        samples[idx.min(n - 1)]
    };
    let mode_label = if cold { "scroll-cold" } else { "scroll" };
    println!(
        r#"{{"mode":"{}","path":"{}","ticks":{},"start_offset":{},"step":{},"sb_len":{},"p50_ns":{},"p95_ns":{},"p99_ns":{},"min_ns":{},"max_ns":{}}}"#,
        mode_label,
        path,
        n,
        actual_start,
        step,
        sb_len,
        p(0.50),
        p(0.95),
        p(0.99),
        samples[0],
        samples[n - 1],
    );
}


//! `--bench chrome-change:<n>:<what>` — the window `--bench
//! chrome-render` draws, with one thing changing every frame:
//!
//! - `sidebar`: a pane's name in the sidebar
//! - `title`: a pane's name in its title strip
//! - `hover`: the pointer, from one sidebar row to another
//! - `move`: the pointer only, with no frame: what following it costs
//!
//! The first three print what `chrome-render` prints; `move` prints
//! the time of one pointer move, from batches of 100.

use marspot::render_metal::ChromeShape;

use crate::bench_chrome::{scene, sidebar, views, NAMES};

const MOVES: usize = 100;

pub fn run(arg: &str) {
    let (n, what) = arg.split_once(':').unwrap_or((arg, ""));
    let Ok(n) = n.parse::<u32>() else {
        eprintln!("bench: chrome-change needs <iterations>:<sidebar|title|hover|move>");
        std::process::exit(2);
    };
    let (mut renderer, target, layout, terminals) = scene();
    let mut views = views(&terminals);
    let mut sidebar = sidebar();
    let mut wr = marspot::render_metal::WindowRender::new();
    let focused = 4;
    let row_h = marspot::layout::sidebar_row_h();
    let row = |i: usize| (20.0, layout.top_inset + layout.sidebar_top_pad_phys + (i as f64 + 0.5) * row_h);
    let hover = |wr: &mut marspot::render_metal::WindowRender,
                     renderer: &mut marspot::render_metal::MetalRenderer,
                     views: &[marspot::render::SessionView<'_>],
                     p: (f64, f64)| {
        let strips: &[marspot::render::SessionView<'_>] = views;
        let shape = ChromeShape { layout: &layout, rows: NAMES.len(), strips: &strips };
        wr.chrome_pointer_move(renderer.font_mut(), &shape, p)
    };
    for _ in 0..5 {
        renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &sidebar, focused);
    }
    if what == "move" {
        // across the toolbar, the sidebar's rows and buttons and a pane
        let points: Vec<(f64, f64)> =
            (0..64).map(|k| ((k * 29 % 300) as f64 + 0.5, (k * 41 % 700) as f64 + 0.5)).collect();
        let mut per: Vec<u64> = Vec::with_capacity(n as usize);
        let mut lit = 0u64;
        for b in 0..n as usize {
            let t0 = std::time::Instant::now();
            for k in 0..MOVES {
                lit += hover(&mut wr, &mut renderer, &views, points[(b * MOVES + k) % points.len()]) as u64;
            }
            per.push(t0.elapsed().as_nanos() as u64 / MOVES as u64);
        }
        per.sort_unstable();
        let q = |f: f64| per[((per.len() as f64 * f) as usize).min(per.len() - 1)];
        println!(r#"{{"mode":"chrome-change","what":"move","batches":{n},"move_p50_ns":{},"move_p99_ns":{},"lit_changes":{lit}}}"#, q(0.5), q(0.99));
        return;
    }
    let mut total: Vec<u64> = Vec::with_capacity(n as usize);
    let mut build: Vec<u64> = Vec::with_capacity(n as usize);
    let mut chrome: Vec<u64> = Vec::with_capacity(n as usize);
    for i in 0..n {
        let odd = i % 2 == 1;
        match what {
            "sidebar" => sidebar[2].label = if odd { "insight" } else { NAMES[2].0 },
            "title" => views[2].title = if odd { "insight" } else { NAMES[2].0 },
            "hover" => {
                hover(&mut wr, &mut renderer, &views, row(if odd { 5 } else { 3 }));
            }
            _ => {
                eprintln!("bench: chrome-change: `{what}` is not sidebar, title, hover or move");
                std::process::exit(2);
            }
        }
        let t0 = std::time::Instant::now();
        renderer.render_layout_to_texture(&mut wr, &target, &layout, &views, &sidebar, focused);
        total.push(t0.elapsed().as_nanos() as u64);
        let sp = renderer.last_render_split();
        build.push(sp.build_us);
        chrome.push(sp.build_us.saturating_sub(sp.build_panes_us));
    }
    let pct = |v: &mut Vec<u64>, q: f64| -> u64 {
        v.sort_unstable();
        v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
    };
    println!(
        r#"{{"mode":"chrome-change","what":"{what}","iterations":{n},"p50_ns":{},"p99_ns":{},"build_p50_us":{},"build_p99_us":{},"chrome_p50_us":{},"chrome_p99_us":{}}}"#,
        pct(&mut total, 0.50),
        pct(&mut total, 0.99),
        pct(&mut build, 0.50),
        pct(&mut build, 0.99),
        pct(&mut chrome, 0.50),
        pct(&mut chrome, 0.99),
    );
}

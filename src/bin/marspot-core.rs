//! marspot-core — Step 2 of the silent-update split.
//!
//! The renderer-and-logic half of the silent-update architecture.
//! Spawned by `marspot-shell` as a child; renders the marspot UI into
//! the shared IOSurface the shell created, then sleeps until the next
//! frame.  The shell composites that IOSurface to its NSWindow.
//!
//! Step 2 scope: minimum-viable real renderer — attach to one shelld
//! session, draw a 1×1 grid with that session's terminal output, run
//! at ~60 fps.  No input forwarding (Step 3), no resize negotiation
//! (Step 4), no sidebar / 9-grid / focus juggling (Step 2+).
//!
//! Input forwarding still routes through the marspot main binary
//! until Step 3 wires the control socket — for now this binary can
//! only *display* a session, not type into it.  Useful for verifying
//! the IOSurface render path works against the real pipeline before
//! the rest of the architecture lands on top.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;

use marspot::font_cache::FontCache;
use marspot::iosurface::IOSurface;
use marspot::layout::Layout;
use marspot::pane::Pane;
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::MetalRenderer;
use marspot::session::SessionState;
use marspot::shell_proto::{
    ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH,
};
use marspot::shelld_client::{default_socket_path, ShelldClient};
use marspot::HEADER_PT;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const FRAME_MS: u64 = 16;

fn env_required<T: std::str::FromStr>(name: &str) -> T {
    let raw = std::env::var(name)
        .unwrap_or_else(|_| panic!("[core] missing required env var {name}"));
    raw.parse::<T>()
        .ok()
        .unwrap_or_else(|| panic!("[core] env {name} = {raw:?} failed to parse"))
}

fn main() {
    eprintln!(
        "marspot-core {} (git {} built {})  pid={}",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown"),
        std::process::id()
    );

    let surface_id: u32 = env_required(ENV_SURFACE_ID);
    let w_phys: f64 = env_required(ENV_SURFACE_WIDTH);
    let h_phys: f64 = env_required(ENV_SURFACE_HEIGHT);
    let scale: f64 = env_required(ENV_SURFACE_SCALE);

    eprintln!("[core] attaching surface {surface_id} ({w_phys}×{h_phys} @ {scale}x)");

    let surface = IOSurface::lookup(surface_id)
        .unwrap_or_else(|| panic!("[core] IOSurfaceLookup({surface_id}) returned nil"));
    surface.increment_use();

    let mut renderer = MetalRenderer::new_headless().expect("[core] MetalRenderer::new_headless");
    let target_tex: objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> = surface
        .make_metal_texture(renderer.device())
        .expect("[core] make_metal_texture");

    // Dirty flag: shelld DATA frames wake us; the render loop polls
    // it as a hint to "render right now" instead of waiting for the
    // next frame tick.  Always render at the frame cadence regardless
    // — DATA bursts arriving during a frame still get composited at
    // the next tick, no extra latency.
    let dirty = Arc::new(AtomicBool::new(true));
    let dirty_for_wake = Arc::clone(&dirty);
    let wake = move || {
        dirty_for_wake.store(true, Ordering::Release);
    };

    let shelld_sock = default_socket_path();
    eprintln!("[core] connecting to shelld at {}", shelld_sock.display());
    let client = match ShelldClient::connect(&shelld_sock, wake) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("[core] shelld connect failed: {e}");
            return;
        }
    };

    // Attach to the first surviving session if there is one, else
    // create a fresh shell.  Mirrors the marspot main bin's bootstrap
    // (src/main.rs phase 4b path) just for one pane.
    let existing: Vec<marspot::shelld_proto::SessionInfo> = client
        .list_sessions()
        .unwrap_or_else(|e| {
            eprintln!("[core] list_sessions failed: {e} — starting fresh");
            Vec::new()
        })
        .into_iter()
        .filter(|s| s.alive)
        .collect();

    let mut pane: Pane = if let Some(info) = existing.first() {
        eprintln!("[core] attaching existing session id={}", info.session_id);
        match client.attach(info.session_id, INITIAL_COLS, INITIAL_ROWS) {
            Ok(s) => Pane::new_shelld(s),
            Err(e) => {
                eprintln!(
                    "[core] attach {} failed: {e} — creating new instead",
                    info.session_id
                );
                let s = client
                    .new_session(INITIAL_COLS, INITIAL_ROWS, "")
                    .expect("[core] new_session");
                Pane::new_shelld(s)
            }
        }
    } else {
        eprintln!("[core] no surviving sessions — creating fresh");
        let s = client
            .new_session(INITIAL_COLS, INITIAL_ROWS, "")
            .expect("[core] new_session");
        Pane::new_shelld(s)
    };

    // FontCache lives in the renderer, but Layout::build wants
    // (cell_w, cell_h) up front to compute per-cell cols/rows.  Build
    // an independent FontCache here to ask for those dims; cheap
    // (small CoreText cache) and the renderer has its own copy.
    let font = FontCache::build().expect("[core] FontCache::build");
    let (cell_w, cell_h) = font.cell_dims();
    let top_inset = HEADER_PT * scale;
    let cell_title_h = 0.0; // single-pane: no per-cell title strip
    let layout = Layout::build(
        w_phys,
        h_phys,
        0.0, // no sidebar in step-2 minimum
        top_inset,
        cell_title_h,
        1,
        1,
        cell_w,
        cell_h,
    );

    eprintln!(
        "[core] layout 1×1 cell={cell_w:.1}×{cell_h:.1} → {} cols × {} rows",
        layout.cells[0].cols, layout.cells[0].rows
    );

    // Now that we know the cell size, resize the pane's terminal so
    // its grid matches what the layout expects.  Without this, the
    // session keeps echoing into an 80×24 grid that doesn't line up
    // with the rendered cell — output wraps at the wrong column.
    pane.resize(layout.cells[0].cols, layout.cells[0].rows);

    eprintln!("[core] entering render loop @ ~{} fps", 1000 / FRAME_MS);

    let start = Instant::now();
    let mut frame: u64 = 0;
    loop {
        let frame_start = Instant::now();
        let _was_dirty = dirty.swap(false, Ordering::AcqRel);
        // Drain any pending shelld DATA into the terminal grid.
        // Pump is cheap when there's nothing pending; called every
        // frame so we never miss bytes between two redraws.
        pane.pump();

        let title_buf = "marspot".to_string();
        let view: SessionView = pane.view(true, &title_buf);
        let views = [view];
        let sidebar: [SidebarEntry; 0] = [];

        renderer.render_layout_to_texture(&target_tex, &layout, &views, &sidebar, 0);

        frame += 1;
        if frame.is_multiple_of(120) {
            let t = start.elapsed().as_secs_f64();
            let state = match pane.session().state() {
                SessionState::Active => "active",
                SessionState::Idle => "idle",
                SessionState::Exited => "exited",
            };
            eprintln!(
                "[core] frame {frame} t={t:.1}s session={state} grid={}x{}",
                pane.session().terminal().grid().cols(),
                pane.session().terminal().grid().rows(),
            );
        }

        let elapsed = frame_start.elapsed();
        let target = Duration::from_millis(FRAME_MS);
        if elapsed < target {
            std::thread::sleep(target - elapsed);
        }
    }
}

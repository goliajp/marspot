//! marspot-core — the renderer + input-dispatch half of the
//! silent-update split.
//!
//! Spawned by `marspot-shell` as a child; renders the marspot UI into
//! the shared IOSurface the shell created, attaches to one shelld
//! session, and processes input forwarded over the control socket.
//!
//! Step 3 scope (this file):
//!   - 1×1 grid + one shelld session (same as Step 2)
//!   - reads `KeyEvent` / `MouseDown` / `Drag` / `Up` / `Scroll` /
//!     `Focus` / `Resize` / `Preedit` frames from fd 3
//!   - dispatches `KeyEvent` into `Pane::handle_key` so typing works
//!   - other events are accepted but not yet applied to layout/state
//!     beyond what's needed for Step 3 typing to feel right
//!
//! Not in scope yet:
//!   - resize → relayout (Step 4)
//!   - 9-grid / sidebar / focus juggling
//!   - selection / IME preedit rendering (mouse events received but
//!     selection logic not wired through)

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::runtime::ProtocolObject;
use objc2_metal::MTLTexture;

use marspot::font_cache::FontCache;
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::iosurface::IOSurface;
use marspot::layout::Layout;
use marspot::pane::Pane;
use marspot::render::{SessionView, SidebarEntry};
use marspot::render_metal::MetalRenderer;
use marspot::session::SessionState;
use marspot::shell_proto::{
    decode_focus, decode_key_event, decode_mouse, decode_preedit, decode_resize, decode_scroll,
    mods_to_struct, wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
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

/// Input event the reader thread converts each control-socket frame
/// into.  The main loop drains a channel of these once per render
/// frame and dispatches them into the focused pane.
#[derive(Debug)]
enum CoreEvent {
    Key(MarspotKeyEvent, Modifiers),
    MouseDown(f64, f64, Modifiers),
    MouseDrag(f64, f64),
    MouseUp(f64, f64),
    Scroll(f64, f64, bool),
    Focus(bool),
    Resize(f64, f64, f64),
    Preedit(String),
    /// Shell closed the control socket — supervisor will tear us down.
    Closed,
}

fn decode_frame(f: &Frame) -> Option<CoreEvent> {
    match f.msg_type {
        MsgType::KeyEvent => decode_key_event(&f.payload).ok().map(|w| {
            let (e, m) = wire_to_event(w);
            CoreEvent::Key(e, m)
        }),
        MsgType::MouseDown => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, m)| CoreEvent::MouseDown(x, y, mods_to_struct(m))),
        MsgType::MouseDrag => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _)| CoreEvent::MouseDrag(x, y)),
        MsgType::MouseUp => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _)| CoreEvent::MouseUp(x, y)),
        MsgType::Scroll => decode_scroll(&f.payload)
            .ok()
            .map(|(dx, dy, p)| CoreEvent::Scroll(dx, dy, p)),
        MsgType::Focus => decode_focus(&f.payload).ok().map(CoreEvent::Focus),
        MsgType::Resize => decode_resize(&f.payload)
            .ok()
            .map(|(w, h, s)| CoreEvent::Resize(w, h, s)),
        MsgType::Preedit => decode_preedit(&f.payload).ok().map(CoreEvent::Preedit),
        // Lifecycle frames don't surface as input events; ignore for
        // now (HELLO/HELLO_ACK get handled inline once we add the
        // handshake in Step 5).
        _ => None,
    }
}

/// Background reader: reads framed messages off the control socket
/// until EOF / error, dispatches each into `tx`, and bumps `dirty`
/// so the render loop wakes up the next tick.
fn reader_loop(mut stream: UnixStream, tx: Sender<CoreEvent>, dirty: Arc<AtomicBool>) {
    loop {
        match Frame::read_from(&mut stream) {
            Ok(None) => {
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
            Ok(Some(frame)) => {
                if let Some(ev) = decode_frame(&frame) {
                    dirty.store(true, Ordering::Release);
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("[core] control read error: {e}");
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
        }
    }
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

    // Bring up the shell ↔ core control socket inherited as fd 3.
    // Frames sent by the shell arrive on the reader thread; the
    // render loop drains the channel once per tick.
    let control_fd: RawFd = std::env::var(ENV_CONTROL_FD)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTROL_FD);
    eprintln!("[core] taking control socket from fd {control_fd}");
    let control_stream = unsafe { UnixStream::from_raw_fd(control_fd) };
    let reader_stream = control_stream
        .try_clone()
        .expect("[core] try_clone control_stream");
    let (event_tx, event_rx): (Sender<CoreEvent>, Receiver<CoreEvent>) = mpsc::channel();
    let dirty_for_reader = Arc::clone(&dirty);
    std::thread::spawn(move || reader_loop(reader_stream, event_tx, dirty_for_reader));

    eprintln!("[core] entering render loop @ ~{} fps", 1000 / FRAME_MS);

    let start = Instant::now();
    let mut frame: u64 = 0;
    let mut key_count: u64 = 0;
    'main: loop {
        let frame_start = Instant::now();
        let _was_dirty = dirty.swap(false, Ordering::AcqRel);
        // Drain pending control-socket events first, then PTY bytes.
        // Order matters: a key press should take effect before the
        // PTY echo lands on the same frame.
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                CoreEvent::Key(event, mods) => {
                    pane.handle_key(&event, mods);
                    key_count += 1;
                }
                CoreEvent::Closed => {
                    eprintln!("[core] control socket closed by shell; exiting render loop");
                    break 'main;
                }
                // Accepted but no-op for now — Step 4 wires resize,
                // selection / IME come in later steps.
                CoreEvent::MouseDown(_, _, _)
                | CoreEvent::MouseDrag(_, _)
                | CoreEvent::MouseUp(_, _)
                | CoreEvent::Scroll(_, _, _)
                | CoreEvent::Focus(_)
                | CoreEvent::Resize(_, _, _)
                | CoreEvent::Preedit(_) => {}
            }
        }
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
        if frame.is_multiple_of(300) {
            let t = start.elapsed().as_secs_f64();
            let state = match pane.session().state() {
                SessionState::Active => "active",
                SessionState::Idle => "idle",
                SessionState::Exited => "exited",
            };
            eprintln!(
                "[core] frame {frame} t={t:.1}s session={state} grid={}x{} keys_dispatched={key_count}",
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

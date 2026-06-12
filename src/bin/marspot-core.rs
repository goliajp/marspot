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
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
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
    decode_focus, decode_hello, decode_key_event, decode_mouse, decode_ping, decode_preedit,
    decode_resize, decode_scroll, encode_hello_ack, encode_pong, encode_surface_ready,
    mods_to_struct, wire_to_event, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
    ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH, PROTO_VERSION,
};
use marspot::shelld_client::{default_socket_path, ShelldClient};
use marspot::HEADER_PT;


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
// Tuple fields on Mouse/Scroll/Focus/Preedit are wired (decoded from
// the wire) but not yet consumed by `Pane` — Step 4 lands resize +
// pump path; selection/IME/scroll/focus follow in later steps.
#[allow(dead_code)]
enum CoreEvent {
    Key(MarspotKeyEvent, Modifiers),
    MouseDown(f64, f64, Modifiers),
    MouseDrag(f64, f64),
    MouseUp(f64, f64),
    Scroll(f64, f64, bool),
    Focus(bool),
    Resize(u32, f64, f64, f64),
    Preedit(String),
    /// Shelld wake — pane has new bytes to pump (PTY → bytelog →
    /// broadcast).  Sent by the shelld client's wake callback so the
    /// main loop is event-driven instead of polling at FRAME_MS.
    PumpShelld,
    /// Shell sent HELLO with its protocol version.  We reply with
    /// HELLO_ACK echoing the version we agree on.
    Hello(u32),
    /// Shell sent a liveness probe.  We echo the nonce back via PONG.
    Ping(u32),
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
            .map(|(id, w, h, s)| CoreEvent::Resize(id, w, h, s)),
        MsgType::Preedit => decode_preedit(&f.payload).ok().map(CoreEvent::Preedit),
        MsgType::Hello => decode_hello(&f.payload).ok().map(CoreEvent::Hello),
        MsgType::Ping => decode_ping(&f.payload).ok().map(CoreEvent::Ping),
        _ => None,
    }
}

/// Background reader: reads framed messages off the control socket
/// until EOF / error, dispatches each into `tx`.  `dirty` is kept for
/// signal-symmetry with the shelld wake closure but unused in the
/// new event-driven loop — sending into `tx` is enough to wake the
/// main thread.
fn reader_loop(mut stream: UnixStream, tx: Sender<CoreEvent>, _dirty: Arc<AtomicBool>) {
    loop {
        match Frame::read_from(&mut stream) {
            Ok(None) => {
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
            Ok(Some(frame)) => {
                if let Some(ev) = decode_frame(&frame) {
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
    let mut w_phys: f64 = env_required(ENV_SURFACE_WIDTH);
    let mut h_phys: f64 = env_required(ENV_SURFACE_HEIGHT);
    let mut scale: f64 = env_required(ENV_SURFACE_SCALE);

    eprintln!("[core] attaching surface {surface_id} ({w_phys}×{h_phys} @ {scale}x)");

    let mut surface = IOSurface::lookup(surface_id)
        .unwrap_or_else(|| panic!("[core] IOSurfaceLookup({surface_id}) returned nil"));
    surface.increment_use();

    let mut renderer = MetalRenderer::new_headless().expect("[core] MetalRenderer::new_headless");
    let mut target_tex: objc2::rc::Retained<ProtocolObject<dyn MTLTexture>> = surface
        .make_metal_texture(renderer.device())
        .expect("[core] make_metal_texture");

    // Unified event channel: the control-socket reader pushes
    // CoreEvents; the shelld wake callback pushes `PumpShelld`.
    // Main loop blocks on `recv_timeout` so it sleeps until *any*
    // event arrives — no polling-cadence latency.  Idle CPU = 0;
    // a key press / shelld frame / window resize wakes us within
    // microseconds rather than waiting for the next frame tick.
    let (event_tx, event_rx): (Sender<CoreEvent>, Receiver<CoreEvent>) = mpsc::channel();
    let dirty = Arc::new(AtomicBool::new(true)); // kept for reader_loop signature
    let event_tx_for_wake = event_tx.clone();
    let wake = move || {
        // Best-effort send; if receiver is gone, the loop is shutting
        // down and we don't care.
        let _ = event_tx_for_wake.send(CoreEvent::PumpShelld);
    };

    // Compute the layout BEFORE we touch shelld, so we can hand
    // the right cols/rows to `new_session` / `attach` from the start.
    // If we created the session at INITIAL_COLS×INITIAL_ROWS and
    // resized it after, the shell would have already printed its
    // welcome message + prompt into the smaller grid; after the
    // resize the leftover content sits at the wrong columns and
    // reads as "phantom indentation" in the rendered view.
    let font = FontCache::build().expect("[core] FontCache::build");
    let (cell_w, cell_h) = font.cell_dims();
    let cell_title_h = 0.0; // single-pane: no per-cell title strip
    let build_layout = |w: f64, h: f64, scale: f64| -> Layout {
        Layout::build(
            w,
            h,
            0.0, // no sidebar in step-4 minimum
            HEADER_PT * scale,
            cell_title_h,
            1,
            1,
            cell_w,
            cell_h,
        )
    };
    let mut layout = build_layout(w_phys, h_phys, scale);
    eprintln!(
        "[core] layout 1×1 cell={cell_w:.1}×{cell_h:.1} → {} cols × {} rows",
        layout.cells[0].cols, layout.cells[0].rows
    );
    let init_cols = layout.cells[0].cols;
    let init_rows = layout.cells[0].rows;

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
        match client.attach(info.session_id, init_cols, init_rows) {
            Ok(s) => Pane::new_shelld(s),
            Err(e) => {
                eprintln!(
                    "[core] attach {} failed: {e} — creating new instead",
                    info.session_id
                );
                let s = client
                    .new_session(init_cols, init_rows, "")
                    .expect("[core] new_session");
                Pane::new_shelld(s)
            }
        }
    } else {
        eprintln!("[core] no surviving sessions — creating fresh");
        let s = client
            .new_session(init_cols, init_rows, "")
            .expect("[core] new_session");
        Pane::new_shelld(s)
    };

    // Bring up the shell ↔ core control socket inherited as fd 3.
    // Frames sent by the shell arrive on the reader thread; the
    // render loop drains the channel once per tick.  The writer half
    // lives in `control_writer` and is shared back to the main loop
    // so we can send `SurfaceReady` after a successful resize.
    let control_fd: RawFd = std::env::var(ENV_CONTROL_FD)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONTROL_FD);
    eprintln!("[core] taking control socket from fd {control_fd}");
    let control_stream = unsafe { UnixStream::from_raw_fd(control_fd) };
    let reader_stream = control_stream
        .try_clone()
        .expect("[core] try_clone control_stream");
    let mut control_writer = control_stream;
    let reader_tx = event_tx.clone();
    let dirty_for_reader = Arc::clone(&dirty);
    std::thread::spawn(move || reader_loop(reader_stream, reader_tx, dirty_for_reader));

    eprintln!("[core] entering event loop (event-driven, no fixed cadence)");

    let start = Instant::now();
    let mut frame: u64 = 0;
    let mut key_count: u64 = 0;
    let mut needs_render = true; // force the first frame so the IOSurface isn't black
    // The first iteration draws immediately even if no event arrives.
    let mut first_tick = true;
    'main: loop {
        // Block until at least one event arrives.  Generous timeout
        // so the loop ticks at most once a second when totally idle
        // — lets the stats line print and lets us notice a stuck
        // state without burning CPU.
        let first = if first_tick {
            first_tick = false;
            // Skip blocking on the very first iteration so the
            // initial render fires immediately.
            event_rx.try_recv().ok()
        } else {
            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ev) => Some(ev),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break 'main,
            }
        };
        // Drain pending control-socket events.  Resize coalescing:
        // during a live window drag the shell fires a Resize per
        // AppKit tick (~60/s), each carrying a fresh IOSurface.
        // Processing every one would queue ~60 SIGWINCHes into the
        // PTY and zsh repaints would lag by a second.  We walk the
        // queue, keep only the *latest* Resize, and skip the older
        // ones — the shell ignores SurfaceReady whose ID doesn't
        // match its current pending surface, so the dropped ones are
        // a no-op for it too.
        let mut pending_resize: Option<(u32, f64, f64, f64)> = None;
        // Liveness frames are echoed within the same drain pass so
        // the shell sees a Pong within microseconds of its Ping.
        let mut to_ack: Vec<(MsgType, Vec<u8>)> = Vec::new();
        let mut closed = false;
        let mut process = |ev: CoreEvent,
                           pane: &mut Pane,
                           pending_resize: &mut Option<(u32, f64, f64, f64)>,
                           key_count: &mut u64,
                           needs_render: &mut bool,
                           to_ack: &mut Vec<(MsgType, Vec<u8>)>,
                           closed: &mut bool| {
            match ev {
                CoreEvent::Key(event, mods) => {
                    pane.handle_key(&event, mods);
                    *key_count += 1;
                    *needs_render = true;
                }
                CoreEvent::Closed => *closed = true,
                CoreEvent::Resize(new_id, new_w, new_h, new_scale) => {
                    *pending_resize = Some((new_id, new_w, new_h, new_scale));
                }
                CoreEvent::PumpShelld => {
                    *needs_render = true;
                }
                CoreEvent::Hello(v) => {
                    to_ack.push((MsgType::HelloAck, encode_hello_ack(v.min(PROTO_VERSION))));
                }
                CoreEvent::Ping(nonce) => {
                    to_ack.push((MsgType::Pong, encode_pong(nonce)));
                }
                CoreEvent::MouseDown(_, _, _)
                | CoreEvent::MouseDrag(_, _)
                | CoreEvent::MouseUp(_, _)
                | CoreEvent::Scroll(_, _, _)
                | CoreEvent::Focus(_)
                | CoreEvent::Preedit(_) => {}
            }
        };
        if let Some(ev) = first {
            process(
                ev,
                &mut pane,
                &mut pending_resize,
                &mut key_count,
                &mut needs_render,
                &mut to_ack,
                &mut closed,
            );
        }
        while let Ok(ev) = event_rx.try_recv() {
            process(
                ev,
                &mut pane,
                &mut pending_resize,
                &mut key_count,
                &mut needs_render,
                &mut to_ack,
                &mut closed,
            );
        }
        if closed {
            eprintln!("[core] control socket closed by shell; exiting event loop");
            break 'main;
        }
        for (ty, payload) in to_ack.drain(..) {
            let frame = Frame::new(ty, payload);
            if let Err(e) = frame.write_to(&mut control_writer) {
                eprintln!("[core] liveness ack {:?} write failed: {e}", ty);
            }
        }
        if let Some((new_id, new_w, new_h, new_scale)) = pending_resize {
            // Shell hands us a freshly-created IOSurface at the new
            // size; rebuild the render target, layout, and pane size,
            // then ack with SurfaceReady so the shell can swap its
            // presenter.
            let new_surface = match IOSurface::lookup(new_id) {
                Some(s) => {
                    s.increment_use();
                    Some(s)
                }
                None => {
                    eprintln!(
                        "[core] Resize: IOSurfaceLookup({new_id}) returned nil; dropping"
                    );
                    None
                }
            };
            if let Some(new_surface) = new_surface {
                match new_surface.make_metal_texture(renderer.device()) {
                    Ok(new_tex) => {
                        target_tex = new_tex;
                        surface.decrement_use();
                        surface = new_surface;
                        w_phys = new_w;
                        h_phys = new_h;
                        scale = new_scale;
                        layout = build_layout(w_phys, h_phys, scale);
                        pane.resize(layout.cells[0].cols, layout.cells[0].rows);
                        // Render the latest content into the new surface
                        // so the SurfaceReady ack is honest.
                        pane.pump();
                        let title_now = "marspot".to_string();
                        let view: SessionView = pane.view(true, &title_now);
                        renderer.render_layout_to_texture(
                            &target_tex,
                            &layout,
                            &[view],
                            &[],
                            0,
                        );
                        let ack = Frame::new(
                            MsgType::SurfaceReady,
                            encode_surface_ready(new_id),
                        );
                        if let Err(e) = ack.write_to(&mut control_writer) {
                            eprintln!("[core] SurfaceReady write failed: {e}");
                        }
                        needs_render = false;
                    }
                    Err(e) => {
                        eprintln!("[core] Resize: make_metal_texture failed: {e}");
                    }
                }
            }
        }
        // Drain any pending shelld DATA into the terminal grid.
        // Pump is cheap when there's nothing pending; called every
        // frame so we never miss bytes between two redraws.
        let pumped = pane.pump();
        if pumped > 0 {
            needs_render = true;
        }

        if needs_render {
            let title_buf = "marspot".to_string();
            let view: SessionView = pane.view(true, &title_buf);
            let views = [view];
            let sidebar: [SidebarEntry; 0] = [];
            renderer.render_layout_to_texture(&target_tex, &layout, &views, &sidebar, 0);
            needs_render = false;
        }

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

        // No sleep — event-driven.  Top of loop blocks on
        // `event_rx.recv_timeout` until the next event arrives or
        // the 1 s idle timeout expires.
    }
}

mod grid;
mod parser;
mod pty;
mod render;
mod terminal;

use std::borrow::Cow;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use objc2_app_kit::{NSScreen, NSView};
use objc2_foundation::MainThreadMarker;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, Modifiers, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::pty::{Pty, PtyConfig, TerminalSize};
use crate::render::Renderer;
use crate::terminal::Terminal;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("MARS_GIT_SHA");

const GRID_COLS: u16 = 80;
const GRID_ROWS: u16 = 24;

/// Bytes per chunk handed off from the reader thread.  64 KiB is the
/// opportunistic upper bound — for interactive output (a single keystroke
/// echoing back) we send what arrived; for bulk output (`cat`) we keep
/// reading non-blocking until the PTY drains, coalescing into one chunk
/// before incurring the per-event NSRunLoop dispatch cost.
const READ_BUF: usize = 64 * 1024;

/// Bounded capacity of the PTY → main-thread channel.  Once full the reader
/// thread blocks on send, which propagates backpressure to the kernel pipe
/// buffer, which propagates to the child's writes — so a runaway producer
/// can't grow our memory unboundedly (CLAUDE.md "bounded queues").
const PTY_CHANNEL_CAPACITY: usize = 64;

/// Wake-up message from the PTY reader thread to the winit event loop.
/// The bytes themselves travel via a separate mpsc channel so the proxy
/// queue stays small (it just carries empty tokens).
#[derive(Debug)]
enum MarsEvent {
    PtyBytes,
    /// PTY master returned EOF — the child shell exited.  Main loop
    /// should commit the final frame and shut down so Drop runs.
    ShellExited,
}

struct Mars {
    window: Option<Window>,
    renderer: Option<Renderer>,
    terminal: Terminal,
    pty: Pty,
    rx: Receiver<Vec<u8>>,
    /// Latest known modifier state, updated by WindowEvent::ModifiersChanged.
    /// winit's KeyEvent does not carry the live modifier flags on macOS, so
    /// we have to track them out-of-band.
    modifiers: ModifiersState,
    /// Self-instrumentation: when set to `Some(t0)`, the next render that
    /// commits to the layer will measure `t0.elapsed()` as the
    /// keystroke-to-pixel latency and record it.  Cleared after the next
    /// successful layer.setContents.  Off-path entirely when MARS_LATENCY
    /// is unset (taken once at startup → `record_latency`).
    pending_keystroke_t0: Option<std::time::Instant>,
    /// Cumulative latency samples, written to MARS_LATENCY's path on Drop.
    /// Always allocated but only pushed to when `record_latency` is true.
    latency_samples: Vec<u64>,
    record_latency: bool,
    latency_out_path: Option<String>,
    /// MARS_PROFILE counters — set when MARS_PROFILE_OUT is configured.
    /// Counts paths through user_event / RedrawRequested / render / feed
    /// so we can tell whether the live pipeline is render-throttled,
    /// event-throttled, or feed-throttled.
    prof: ProfileCounters,
    profile_out_path: Option<String>,
}

#[derive(Default)]
struct ProfileCounters {
    user_events: u64,
    chunks_drained: u64,
    bytes_fed: u64,
    request_redraws: u64,
    redraw_requested_calls: u64,
    render_calls: u64,
    render_total_ns: u64,
    feed_total_ns: u64,
    drain_total_ns: u64,
    started_at: Option<std::time::Instant>,
}

impl ApplicationHandler<MarsEvent> for Mars {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let title = format!("Mars v{} ({})", VERSION, GIT_SHA);
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        let window = event_loop.create_window(attrs).expect("create window");

        // mainScreen() can return a 1x screen even when our window will
        // land on a 2x one. Survey all screens and use the max so the
        // CALayer is composited at the right density.
        let main_thread = MainThreadMarker::new()
            .expect("Mars must be created on the main thread");
        let screens = NSScreen::screens(main_thread);
        let mut scales: Vec<f32> = Vec::new();
        for i in 0..screens.len() {
            let s = unsafe { screens.objectAtIndex(i) };
            scales.push(s.backingScaleFactor() as f32);
        }
        let max_scale = scales.iter().cloned().fold(1.0_f32, f32::max);
        let scale: f32 = std::env::var("MARS_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(max_scale);

        let renderer = unsafe {
            let handle = window.window_handle().expect("window handle").as_raw();
            let RawWindowHandle::AppKit(appkit) = handle else {
                panic!("Mars only supports the AppKit backend");
            };
            let nsview: &NSView = &*(appkit.ns_view.as_ptr() as *const NSView);
            Renderer::new(nsview, scale).expect("renderer init")
        };

        let size = window.inner_size();
        let phys_w = (size.width as f64) * (scale as f64);
        let phys_h = (size.height as f64) * (scale as f64);
        let mut renderer = renderer;
        renderer.resize(phys_w, phys_h);

        window.request_redraw();

        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: MarsEvent) {
        match event {
            MarsEvent::PtyBytes => {
                self.prof.user_events += 1;
                let drain_t0 = std::time::Instant::now();
                let mut got_any = false;
                while let Ok(chunk) = self.rx.try_recv() {
                    self.prof.chunks_drained += 1;
                    self.prof.bytes_fed += chunk.len() as u64;
                    let feed_t0 = std::time::Instant::now();
                    self.terminal.feed(&chunk);
                    self.prof.feed_total_ns += feed_t0.elapsed().as_nanos() as u64;
                    got_any = true;
                }
                self.prof.drain_total_ns += drain_t0.elapsed().as_nanos() as u64;
                if got_any {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                        self.prof.request_redraws += 1;
                    }
                }
            }
            MarsEvent::ShellExited => {
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                // winit's PhysicalSize on macOS is already physical pixels —
                // do NOT multiply by backingScaleFactor again (that would
                // double-scale on Retina; on the user's 1x display the
                // previous code happened to work because scale=1).
                let phys_w = size.width as f64;
                let phys_h = size.height as f64;
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(phys_w, phys_h);
                    let (cell_w, cell_h) = r.cell_dims();
                    let cols = ((phys_w / cell_w).floor() as u16).max(1);
                    let rows = ((phys_h / cell_h).floor() as u16).max(1);
                    if (cols, rows) != (self.terminal.grid().cols(), self.terminal.grid().rows()) {
                        self.terminal.resize(cols, rows);
                        let _ = self.pty.resize(TerminalSize {
                            cols,
                            rows,
                            pixel_width: phys_w as u16,
                            pixel_height: phys_h as u16,
                        });
                    }
                    // Render synchronously here so the next CA commit lands
                    // a CGImage at the new size; deferring via request_redraw
                    // leaves a one-frame gap during live resize where the
                    // layer shows stale-or-stretched contents.
                    r.render(self.terminal.grid());
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(bytes) = key_event_to_bytes(&event, self.modifiers) {
                    if self.record_latency && self.pending_keystroke_t0.is_none() {
                        self.pending_keystroke_t0 = Some(std::time::Instant::now());
                    }
                    // Typing always snaps view back to live — the user
                    // isn't going to want to send keys while looking at
                    // historical output.
                    if let Some(r) = self.renderer.as_mut() {
                        if r.view_offset() != 0 {
                            r.set_view_offset(0, 0);
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                    }
                    let _ = self.pty.write(&bytes);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Convert delta to lines.  macOS trackpads emit
                // PixelDelta in logical pixels (we divide by cell_h).
                // External wheels emit LineDelta where 1.0 ≈ one notch.
                let cell_h = self
                    .renderer
                    .as_ref()
                    .map(|r| r.cell_dims().1)
                    .unwrap_or(15.0);
                // winit on macOS reports negative y for scroll-up
                // gestures (the user moves the wheel up / swipes up,
                // expecting older content to come into view).  Negate
                // so positive lines_f means "scroll up to see older".
                let lines_f = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -(y as f64) * 3.0,
                    MouseScrollDelta::PixelDelta(p) => -p.y / cell_h,
                };
                if lines_f.abs() < 0.5 {
                    return;
                }
                let max = self.terminal.grid().scrollback_len() as i32;
                let cur = self
                    .renderer
                    .as_ref()
                    .map(|r| r.view_offset() as i32)
                    .unwrap_or(0);
                let new = (cur + lines_f as i32).clamp(0, max) as u16;
                if let Some(r) = self.renderer.as_mut() {
                    r.set_view_offset(new, max as u16);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.prof.redraw_requested_calls += 1;
                if let Some(r) = self.renderer.as_mut() {
                    let render_t0 = std::time::Instant::now();
                    r.render(self.terminal.grid());
                    self.prof.render_total_ns += render_t0.elapsed().as_nanos() as u64;
                    self.prof.render_calls += 1;
                    if self.prof.started_at.is_none() {
                        self.prof.started_at = Some(std::time::Instant::now());
                    }
                    // Latency instrumentation — close the loop opened by
                    // the most recent keystroke.  Renderer::render returns
                    // after layer.setContents, which is the closest
                    // cheap-to-measure proxy for "pixels visible" without
                    // hooking into CoreAnimation's display server.
                    if let Some(t0) = self.pending_keystroke_t0.take() {
                        let ns = t0.elapsed().as_nanos() as u64;
                        self.latency_samples.push(ns);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Map a winit key press to the byte sequence we send the PTY.  Returns
/// `None` for events we don't translate (releases, modifier-only, Cmd
/// combos that the OS handles, etc.).
fn key_event_to_bytes(
    event: &KeyEvent,
    modifiers: ModifiersState,
) -> Option<Cow<'static, [u8]>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    // Cmd combos belong to the OS / app layer (Cmd-Q to quit, Cmd-C/V for
    // clipboard, etc.) — never forward them to the PTY.
    if modifiers.super_key() {
        return None;
    }

    // Ctrl + letter → ASCII control code (Ctrl-A = 0x01 ... Ctrl-Z = 0x1A).
    // Also: Ctrl-[ = ESC, Ctrl-\ = FS, Ctrl-] = GS, Ctrl-^ = RS, Ctrl-_ = US,
    // Ctrl-Space = NUL.  Done before the named-key match so Ctrl-anything
    // takes priority over the per-key text payload.
    if modifiers.control_key() {
        if let Key::Character(s) = &event.logical_key {
            if let Some(c) = s.chars().next() {
                let lc = c.to_ascii_lowercase();
                let code = match lc {
                    'a'..='z' => Some((lc as u8) - b'a' + 1),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' => Some(0x1f),
                    ' ' => Some(0x00),
                    _ => None,
                };
                if let Some(code) = code {
                    return Some(Cow::Owned(vec![code]));
                }
            }
        }
    }

    match &event.logical_key {
        Key::Named(NamedKey::Enter) => Some(Cow::Borrowed(b"\r")),
        Key::Named(NamedKey::Backspace) => Some(Cow::Borrowed(b"\x7f")),
        Key::Named(NamedKey::Tab) => Some(Cow::Borrowed(b"\t")),
        Key::Named(NamedKey::Escape) => Some(Cow::Borrowed(b"\x1b")),
        Key::Named(NamedKey::ArrowUp) => Some(Cow::Borrowed(b"\x1b[A")),
        Key::Named(NamedKey::ArrowDown) => Some(Cow::Borrowed(b"\x1b[B")),
        Key::Named(NamedKey::ArrowRight) => Some(Cow::Borrowed(b"\x1b[C")),
        Key::Named(NamedKey::ArrowLeft) => Some(Cow::Borrowed(b"\x1b[D")),
        _ => event
            .text
            .as_ref()
            .map(|t| Cow::Owned(t.as_bytes().to_vec())),
    }
}

/// Spawn a thread that blocks on `read(master_fd)` and forwards each chunk
/// to the main loop. Closing `master_fd` (e.g. from `Pty::drop` on shutdown)
/// makes the read return ≤0, which terminates the thread cleanly.
fn spawn_pty_reader(
    master_fd: std::os::unix::io::RawFd,
    tx: SyncSender<Vec<u8>>,
    proxy: EventLoopProxy<MarsEvent>,
) {
    thread::Builder::new()
        .name("mars-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                // First read: blocks until at least one byte arrives.
                let n = unsafe {
                    libc::read(
                        master_fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    // EOF or error → child gone or fd closed.  Tell the
                    // main loop so it can commit the final frame and
                    // exit cleanly (lets Drop run).
                    let _ = proxy.send_event(MarsEvent::ShellExited);
                    break;
                }
                let mut total = n as usize;

                // Opportunistic drain: poll(0) and read whatever else is
                // already in the kernel pipe.  Cuts IPC × main-loop
                // dispatch cost on bulk output (cat large file) without
                // adding latency for interactive output (single byte sent
                // immediately because poll says "no more").
                while total < buf.len() {
                    let mut pfd = libc::pollfd {
                        fd: master_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
                    if ready <= 0 {
                        break;
                    }
                    let n2 = unsafe {
                        libc::read(
                            master_fd,
                            buf.as_mut_ptr().add(total) as *mut libc::c_void,
                            buf.len() - total,
                        )
                    };
                    if n2 <= 0 {
                        break;
                    }
                    total += n2 as usize;
                }

                let chunk = buf[..total].to_vec();
                if tx.send(chunk).is_err() {
                    // Main thread dropped the receiver — we're shutting down.
                    break;
                }
                if proxy.send_event(MarsEvent::PtyBytes).is_err() {
                    // Event loop already exited.
                    break;
                }
            }
        })
        .expect("spawn pty reader thread");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = parse_named_arg(&args, "--snapshot") {
        run_snapshot(&path);
        return;
    }
    if let Some(spec) = parse_named_arg(&args, "--bench") {
        run_bench(&spec);
        return;
    }

    let event_loop: EventLoop<MarsEvent> = EventLoop::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // MARS_SHELL takes precedence over SHELL.  Lets the bench harness
    // point mars at a one-shot script (no keystroke pumping needed) so we
    // get a clean PTY → render measurement without zsh-init interference.
    let shell = std::env::var("MARS_SHELL")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "/bin/zsh".into());
    let pty = Pty::spawn(PtyConfig {
        program: shell,
        // Pty::spawn already pushes `program` as argv[0]; this vec is for
        // additional args only.
        args: Vec::new(),
        size: TerminalSize {
            cols: GRID_COLS,
            rows: GRID_ROWS,
            pixel_width: 0,
            pixel_height: 0,
        },
    })
    .expect("spawn pty");

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(PTY_CHANNEL_CAPACITY);
    let proxy = event_loop.create_proxy();
    spawn_pty_reader(pty.raw_master(), tx, proxy);

    // MARS_LATENCY=/path/out.json — write a JSON list of keystroke→
    // setContents nanoseconds to the given path on exit.  Off otherwise.
    let latency_out_path = std::env::var("MARS_LATENCY").ok();
    let record_latency = latency_out_path.is_some();

    // MARS_PROFILE=/path/profile.json — write event/render counters and
    // timings on exit so we can tell which loop is actually slow.
    let profile_out_path = std::env::var("MARS_PROFILE").ok();

    let mut app = Mars {
        window: None,
        renderer: None,
        terminal: Terminal::new(GRID_COLS, GRID_ROWS),
        pty,
        rx,
        modifiers: ModifiersState::empty(),
        pending_keystroke_t0: None,
        latency_samples: Vec::new(),
        record_latency,
        latency_out_path,
        prof: ProfileCounters::default(),
        profile_out_path,
    };
    event_loop.run_app(&mut app).expect("run app");
}

impl Drop for Mars {
    fn drop(&mut self) {
        if let Some(path) = self.latency_out_path.take() {
            let mut s = String::with_capacity(self.latency_samples.len() * 12);
            s.push('[');
            for (i, ns) in self.latency_samples.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&ns.to_string());
            }
            s.push(']');
            if let Err(e) = std::fs::write(&path, s) {
                eprintln!("mars: failed to write latency log to {path}: {e}");
            }
        }
        if let Some(path) = self.profile_out_path.take() {
            let p = &self.prof;
            let json = format!(
                r#"{{"user_events":{ue},"chunks_drained":{cd},"bytes_fed":{bf},"request_redraws":{rr},"redraw_requested_calls":{rrc},"render_calls":{rc},"render_total_ns":{rtn},"feed_total_ns":{ftn},"drain_total_ns":{dtn}}}"#,
                ue = p.user_events,
                cd = p.chunks_drained,
                bf = p.bytes_fed,
                rr = p.request_redraws,
                rrc = p.redraw_requested_calls,
                rc = p.render_calls,
                rtn = p.render_total_ns,
                ftn = p.feed_total_ns,
                dtn = p.drain_total_ns,
            );
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("mars: failed to write profile log to {path}: {e}");
            }
        }
    }
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

/// Headless render: build a Renderer with no view, render one frame into
/// an offscreen bitmap, encode as PNG, write to `path`. The terminal is
/// pre-loaded with a demo banner so the snapshot has visible content
/// without needing a live PTY.
fn run_snapshot(path: &str) {
    let scale: f32 = std::env::var("MARS_SNAPSHOT_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            MainThreadMarker::new()
                .and_then(NSScreen::mainScreen)
                .map(|s| s.backingScaleFactor() as f32)
        })
        .unwrap_or(1.0);
    let logical_w: u32 = 960;
    let logical_h: u32 = 600;
    let phys_w = (logical_w as f32 * scale) as u32;
    let phys_h = (logical_h as f32 * scale) as u32;

    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    feed_demo_content(&mut terminal);

    let mut renderer = Renderer::new_offscreen(scale).expect("offscreen renderer");
    renderer.resize(phys_w as f64, phys_h as f64);
    let bgra = renderer
        .snapshot(phys_w, phys_h, terminal.grid())
        .expect("snapshot");

    let mut rgba = bgra.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }

    let file = std::fs::File::create(path).expect("create snapshot file");
    let buf_writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(buf_writer, phys_w, phys_h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&rgba).expect("png write");

    eprintln!(
        "wrote snapshot: {} ({}x{} physical, scale={})",
        path, phys_w, phys_h, scale
    );
}

/// Headless benchmark dispatcher.  Spec is `<mode>:<arg>`.
///
/// Modes:
///   parse:<path>   feed bytes from `path` through Terminal::feed and
///                  report wall-clock throughput (bytes/sec)
///   render:<n>     run `n` full-frame renders against a synthetic
///                  worst-case grid (full coloured cells), report
///                  per-frame p50/p95/p99 nanoseconds
///
/// Both modes write a single line of JSON to stdout so harness scripts
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
        "render" => bench_render(arg),
        other => {
            eprintln!("unknown bench mode: {other}");
            std::process::exit(2);
        }
    }
}

fn bench_parse(path: &str) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("bench: read {path}: {e}");
        std::process::exit(2);
    });
    // Use the same grid dimensions as a typical mars window (auto-fit
    // 122×39 on the user's default 960×600 layout) so the parser path
    // exercises wrap / scroll the way it does in real use.
    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    let t0 = std::time::Instant::now();
    terminal.feed(&bytes);
    let elapsed_ns = t0.elapsed().as_nanos() as u64;
    let bytes_per_sec = if elapsed_ns > 0 {
        (bytes.len() as u128 * 1_000_000_000 / elapsed_ns as u128) as u64
    } else {
        0
    };
    println!(
        r#"{{"mode":"parse","path":"{}","bytes":{},"elapsed_ns":{},"bytes_per_sec":{}}}"#,
        path,
        bytes.len(),
        elapsed_ns,
        bytes_per_sec
    );
}

fn bench_render(arg: &str) {
    let n: u32 = arg.parse().unwrap_or_else(|_| {
        eprintln!("bench: render needs an integer iteration count");
        std::process::exit(2);
    });

    // Build a worst-case grid: every cell carries a non-default fg colour
    // (forces a SetRGBFillColor per glyph run), every cell is non-blank
    // (no skipping), and the content alternates printable ASCII so glyph
    // run-length compression has to stop frequently.  This is the upper
    // bound on per-frame cost given the current architecture.
    let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
    // Fill with rotating SGR colours + printable ASCII.
    let mut payload: Vec<u8> = Vec::with_capacity(64 * 1024);
    for r in 0..GRID_ROWS {
        for c in 0..GRID_COLS {
            // SGR 30..=37 cycling foreground.
            let colour = 30 + ((r as u32 + c as u32) % 8) as u8;
            payload.extend_from_slice(format!("\x1b[{}m", colour).as_bytes());
            let ch = ((c % 95) as u8) + 32; // printable ASCII 32..127
            payload.push(ch);
        }
        if r + 1 < GRID_ROWS {
            payload.extend_from_slice(b"\r\n");
        }
    }
    terminal.feed(&payload);

    // Render headlessly into an offscreen Renderer.  Use scale=1 to
    // match the user's display so numbers transfer to the live path.
    let mut renderer = Renderer::new_offscreen(1.0).expect("offscreen renderer");
    // Default 960×600 logical → physical at scale 1.
    let phys_w = 960.0_f64;
    let phys_h = 600.0_f64;

    // Warm-up: 5 iterations to fill char_cache and prime CGContext alloc.
    for _ in 0..5 {
        let _ = renderer.snapshot(phys_w as u32, phys_h as u32, terminal.grid());
    }

    let mut samples: Vec<u64> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let t0 = std::time::Instant::now();
        let _ = renderer.snapshot(phys_w as u32, phys_h as u32, terminal.grid());
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    let p = |q: f64| -> u64 {
        let idx = ((samples.len() as f64) * q) as usize;
        samples[idx.min(samples.len() - 1)]
    };
    println!(
        r#"{{"mode":"render","iterations":{},"p50_ns":{},"p95_ns":{},"p99_ns":{},"min_ns":{},"max_ns":{}}}"#,
        n,
        p(0.50),
        p(0.95),
        p(0.99),
        samples[0],
        samples[samples.len() - 1],
    );
}

fn feed_demo_content(terminal: &mut Terminal) {
    let banner = format!("mars v{} ({})\r\n", VERSION, GIT_SHA);
    terminal.feed(banner.as_bytes());
    terminal.feed(b"\r\n");
    terminal.feed(b"hello mars\r\n");
    terminal.feed(b"the engine is alive\r\n");
    terminal.feed(b"\r\n");
    terminal.feed(b"  pty + parser + grid + render (CoreText)\r\n");
    terminal.feed(b"\r\n");
    terminal.feed(b"  ascii printable: !\"#$%&'()*+,-./0123456789:;<=>?@\r\n");
    terminal.feed(b"                   ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_\r\n");
    terminal.feed(b"                   `abcdefghijklmnopqrstuvwxyz{|}~\r\n");
}

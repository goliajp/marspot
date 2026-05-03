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
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::pty::{Pty, PtyConfig, TerminalSize};
use crate::render::Renderer;
use crate::terminal::Terminal;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("MARS_GIT_SHA");

const GRID_COLS: u16 = 80;
const GRID_ROWS: u16 = 24;

/// Bytes per chunk handed off from the reader thread.  4 KiB matches the
/// typical pipe buffer granule and keeps allocations small.
const READ_BUF: usize = 4096;

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
}

struct Mars {
    window: Option<Window>,
    renderer: Option<Renderer>,
    terminal: Terminal,
    pty: Pty,
    rx: Receiver<Vec<u8>>,
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

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: MarsEvent) {
        match event {
            MarsEvent::PtyBytes => {
                let mut got_any = false;
                while let Ok(chunk) = self.rx.try_recv() {
                    self.terminal.feed(&chunk);
                    got_any = true;
                }
                if got_any {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
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
                let scale = MainThreadMarker::new()
                    .and_then(NSScreen::mainScreen)
                    .map(|s| s.backingScaleFactor() as f64)
                    .unwrap_or(1.0);
                let phys_w = size.width as f64 * scale;
                let phys_h = size.height as f64 * scale;
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(phys_w, phys_h);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(bytes) = key_event_to_bytes(&event) {
                    let _ = self.pty.write(&bytes);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.render(self.terminal.grid());
                }
            }
            _ => {}
        }
    }
}

/// Map a winit key press to the byte sequence we send the PTY.  Returns
/// `None` for events we don't translate (releases, modifier-only, etc.).
fn key_event_to_bytes(event: &KeyEvent) -> Option<Cow<'static, [u8]>> {
    if event.state != ElementState::Pressed {
        return None;
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
                let n = unsafe {
                    libc::read(
                        master_fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    // EOF or error → child gone or fd closed.
                    break;
                }
                let chunk = buf[..n as usize].to_vec();
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

    let event_loop: EventLoop<MarsEvent> = EventLoop::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
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

    let mut app = Mars {
        window: None,
        renderer: None,
        terminal: Terminal::new(GRID_COLS, GRID_ROWS),
        pty,
        rx,
    };
    event_loop.run_app(&mut app).expect("run app");
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

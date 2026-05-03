mod atlas;
mod grid;
mod parser;
mod pty;
mod render;
mod terminal;

use objc2_app_kit::NSView;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::render::Renderer;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("MARS_GIT_SHA");

#[derive(Default)]
struct Mars {
    window: Option<Window>,
    renderer: Option<Renderer>,
}

impl ApplicationHandler for Mars {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let title = format!("Mars v{} ({})", VERSION, GIT_SHA);
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        let window = event_loop.create_window(attrs).expect("create window");

        // Reach into AppKit to get the NSView backing this winit window so
        // we can hand it a CAMetalLayer.  Safe on macOS — winit's AppKit
        // backend is the only valid path here.
        let scale = window.scale_factor() as f32;
        let renderer = unsafe {
            let handle = window
                .window_handle()
                .expect("window handle")
                .as_raw();
            let RawWindowHandle::AppKit(appkit) = handle else {
                panic!("Mars only supports the AppKit backend");
            };
            let nsview: &NSView = &*(appkit.ns_view.as_ptr() as *const NSView);
            Renderer::new(nsview, scale).expect("renderer init")
        };

        // Initialize the layer's drawable size to the window's pixel size.
        let size = window.inner_size();
        let mut renderer = renderer;
        renderer.resize(size.width as f64, size.height as f64);

        // Trigger an initial paint so the user sees our clear color
        // immediately rather than the system default.
        window.request_redraw();

        self.window = Some(window);
        self.renderer = Some(renderer);
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
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width as f64, size.height as f64);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    r.render();
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = parse_snapshot_arg(&args) {
        run_snapshot(&path);
        return;
    }

    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = Mars::default();
    event_loop.run_app(&mut app).expect("run app");
}

/// Parse `--snapshot <path>` (or `--snapshot=path`).  Returns the path
/// when present.  Anything else (including bare flag with no value) is
/// treated as no snapshot.
fn parse_snapshot_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter().skip(1);
    while let Some(a) = iter.next() {
        if a == "--snapshot" {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix("--snapshot=") {
            return Some(rest.to_string());
        }
    }
    None
}

/// Headless render: build a Renderer with no view, render one frame into
/// an offscreen texture, encode the result as a PNG, write it to `path`,
/// and exit.  The point: visually-verifiable output without screen-capture
/// permissions, window focus, or a graphical session.
fn run_snapshot(path: &str) {
    // Match the live default — 960x600 logical at 2x = 1920x1200 physical.
    // Hardcoded for now; later we'll let the caller pick.
    let scale: f32 = 2.0;
    let logical_w: u32 = 960;
    let logical_h: u32 = 600;
    let phys_w = (logical_w as f32 * scale) as u32;
    let phys_h = (logical_h as f32 * scale) as u32;

    let mut renderer = Renderer::new_offscreen(scale).expect("offscreen renderer");
    renderer.resize(phys_w as f64, phys_h as f64);
    let bgra = renderer.snapshot(phys_w, phys_h).expect("snapshot");

    // Convert BGRA → RGBA per pixel (PNG wants RGBA).
    let mut rgba = bgra.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2); // B↔R, leave G and A in place
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

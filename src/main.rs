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

#[derive(Default)]
struct Mars {
    window: Option<Window>,
    renderer: Option<Renderer>,
}

impl ApplicationHandler for Mars {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Mars")
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        let window = event_loop.create_window(attrs).expect("create window");

        // Reach into AppKit to get the NSView backing this winit window so
        // we can hand it a CAMetalLayer.  Safe on macOS — winit's AppKit
        // backend is the only valid path here.
        let renderer = unsafe {
            let handle = window
                .window_handle()
                .expect("window handle")
                .as_raw();
            let RawWindowHandle::AppKit(appkit) = handle else {
                panic!("Mars only supports the AppKit backend");
            };
            let nsview: &NSView = &*(appkit.ns_view.as_ptr() as *const NSView);
            Renderer::new(nsview).expect("renderer init")
        };

        // Initialize the layer's drawable size to the window's pixel size.
        let size = window.inner_size();
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
                if let Some(r) = &self.renderer {
                    r.resize(size.width as f64, size.height as f64);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = &self.renderer {
                    r.render();
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = Mars::default();
    event_loop.run_app(&mut app).expect("run app");
}

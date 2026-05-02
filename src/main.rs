mod grid;
mod parser;
mod pty;
mod terminal;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

#[derive(Default)]
struct Mars {
    window: Option<Window>,
}

impl ApplicationHandler for Mars {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Mars")
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        self.window = Some(event_loop.create_window(attrs).expect("create window"));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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

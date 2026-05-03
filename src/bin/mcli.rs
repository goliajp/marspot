//! mcli — minimal standalone single-session terminal.
//!
//! `mcli` is the acid test for the Session API's independence: it owns
//! one Session, one Renderer, one window — nothing else from mars's
//! multi-terminal machinery.  If the Session API can't power mcli
//! cleanly, the Mars container is leaning on private state and the
//! abstraction is wrong.
//!
//! Behaviourally mcli is a pared-down mars: same shell, same fonts,
//! same scrollback, same keyboard/mouse handling — just one cell
//! and no sidebar / layout / multi-session bookkeeping.

use objc2_app_kit::{NSScreen, NSView};
use objc2_foundation::MainThreadMarker;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{Modifiers, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use mars::input::{
    key_event_to_bytes, KeyState, LogicalKey, MarsKeyEvent,
    Modifiers as MarsModifiers, NamedKey as MarsNamedKey,
};
use mars::render::{Renderer, SessionView};
use mars::session::Session;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

#[derive(Debug, Clone)]
enum McliEvent {
    Wake,
}

struct Mcli {
    window: Option<Window>,
    renderer: Option<Renderer>,
    session: Session,
    modifiers: ModifiersState,
    view_offset: u16,
}

impl ApplicationHandler<McliEvent> for Mcli {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let title = format!("mcli — {} {}", env!("CARGO_PKG_VERSION"), env!("MARS_GIT_SHA"));
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(960.0, 600.0));
        let window = event_loop.create_window(attrs).expect("create window");

        let mt = MainThreadMarker::new().expect("main thread");
        let screens = NSScreen::screens(mt);
        let mut max_scale = 1.0_f32;
        for i in 0..screens.len() {
            let s = unsafe { screens.objectAtIndex(i) };
            max_scale = max_scale.max(s.backingScaleFactor() as f32);
        }
        let scale: f32 = std::env::var("MARS_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(max_scale);

        let renderer = unsafe {
            let handle = window.window_handle().expect("window handle").as_raw();
            let RawWindowHandle::AppKit(appkit) = handle else {
                panic!("mcli only supports the AppKit backend");
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: McliEvent) {
        match event {
            McliEvent::Wake => {
                if self.session.pump() > 0 {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                if self.session.is_exited() {
                    self.session.pump(); // commit final bytes
                    event_loop.exit();
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
                let phys_w = size.width as f64;
                let phys_h = size.height as f64;
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(phys_w, phys_h);
                    let (cell_w, cell_h) = r.cell_dims();
                    let cols = ((phys_w / cell_w).floor() as u16).max(1);
                    let rows = ((phys_h / cell_h).floor() as u16).max(1);
                    if (cols, rows)
                        != (
                            self.session.terminal.grid().cols(),
                            self.session.terminal.grid().rows(),
                        )
                    {
                        self.session.resize(cols, rows);
                    }
                    let view = SessionView {
                        grid: self.session.terminal.grid(),
                        view_offset: self.view_offset,
                        cursor_visible: self.session.terminal.cursor_visible(),
                        focused: true,
                    };
                    r.render(view);
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Focused(focused) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.set_window_focused(focused);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let mars_event = winit_to_mars_key_event(&event);
                let mars_mods = winit_to_mars_modifiers(self.modifiers);
                if let Some(bytes) = key_event_to_bytes(&mars_event, mars_mods) {
                    if self.view_offset != 0 {
                        self.view_offset = 0;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    let _ = self.session.write(&bytes);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_h = self
                    .renderer
                    .as_ref()
                    .map(|r| r.cell_dims().1)
                    .unwrap_or(15.0);
                let lines_f = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -(y as f64) * 3.0,
                    MouseScrollDelta::PixelDelta(p) => -p.y / cell_h,
                };
                if lines_f.abs() < 0.5 {
                    return;
                }
                let max = self.session.terminal.grid().scrollback_len() as i32;
                let new = (self.view_offset as i32 + lines_f as i32).clamp(0, max) as u16;
                if new != self.view_offset {
                    self.view_offset = new;
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = self.renderer.as_mut() {
                    let view = SessionView {
                        grid: self.session.terminal.grid(),
                        view_offset: self.view_offset,
                        cursor_visible: self.session.terminal.cursor_visible(),
                        focused: true,
                    };
                    r.render(view);
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let event_loop: EventLoop<McliEvent> = EventLoop::with_user_event()
        .build()
        .expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let wake = move || {
        let _ = proxy.send_event(McliEvent::Wake);
    };

    let session =
        Session::spawn(INITIAL_COLS, INITIAL_ROWS, wake).expect("spawn initial session");

    let mut app = Mcli {
        window: None,
        renderer: None,
        session,
        modifiers: ModifiersState::empty(),
        view_offset: 0,
    };
    event_loop.run_app(&mut app).expect("run app");
}

/// Translate a winit KeyEvent into Mars's portable representation.
/// Mirror of the helper in main.rs — see comment there.  Both go away
/// when winit is dropped in favour of an AppKit-direct event loop.
fn winit_to_mars_key_event(event: &winit::event::KeyEvent) -> MarsKeyEvent {
    use winit::event::ElementState;
    use winit::keyboard::{Key, NamedKey as WNamed};

    let state = match event.state {
        ElementState::Pressed => KeyState::Pressed,
        ElementState::Released => KeyState::Released,
    };
    let logical = match &event.logical_key {
        Key::Character(s) => s
            .chars()
            .next()
            .map(LogicalKey::Char)
            .unwrap_or(LogicalKey::Other),
        Key::Named(WNamed::Enter) => LogicalKey::Named(MarsNamedKey::Enter),
        Key::Named(WNamed::Backspace) => LogicalKey::Named(MarsNamedKey::Backspace),
        Key::Named(WNamed::Tab) => LogicalKey::Named(MarsNamedKey::Tab),
        Key::Named(WNamed::Escape) => LogicalKey::Named(MarsNamedKey::Escape),
        Key::Named(WNamed::ArrowUp) => LogicalKey::Named(MarsNamedKey::ArrowUp),
        Key::Named(WNamed::ArrowDown) => LogicalKey::Named(MarsNamedKey::ArrowDown),
        Key::Named(WNamed::ArrowLeft) => LogicalKey::Named(MarsNamedKey::ArrowLeft),
        Key::Named(WNamed::ArrowRight) => LogicalKey::Named(MarsNamedKey::ArrowRight),
        _ => LogicalKey::Other,
    };
    let text = event.text.as_ref().map(|s| s.as_str().to_string());
    MarsKeyEvent {
        state,
        logical,
        text,
    }
}

fn winit_to_mars_modifiers(m: ModifiersState) -> MarsModifiers {
    MarsModifiers {
        shift: m.shift_key(),
        control: m.control_key(),
        alt: m.alt_key(),
        super_: m.super_key(),
    }
}

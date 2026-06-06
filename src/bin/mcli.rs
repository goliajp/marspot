//! mcli — minimal standalone single-session terminal.
//!
//! `mcli` is the acid test for the Session API's independence: it owns
//! one Session, one Renderer, one window — nothing else from marspot's
//! multi-terminal machinery.  If the Session API can't power mcli
//! cleanly, the Marspot container is leaning on private state and the
//! abstraction is wrong.
//!
//! Behaviourally mcli is a pared-down marspot: same shell, same fonts,
//! same scrollback, same keyboard/mouse handling — just one cell
//! and no sidebar / layout / multi-session bookkeeping.

use objc2_app_kit::NSScreen;
use objc2_foundation::MainThreadMarker;

use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{key_event_to_bytes, MarspotKeyEvent, Modifiers};
use marspot::render::{Renderer, SessionView};
use marspot::session::Session;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

struct Mcli {
    renderer: Option<Renderer>,
    session: Session,
    view_offset: u16,
}

impl MarspotApp for Mcli {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        let mt = MainThreadMarker::new().expect("main thread");
        let screens = NSScreen::screens(mt);
        let mut max_scale = 1.0_f32;
        for i in 0..screens.len() {
            let s = unsafe { screens.objectAtIndex(i) };
            max_scale = max_scale.max(s.backingScaleFactor() as f32);
        }
        let scale: f32 = std::env::var("MARSPOT_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(max_scale);

        let renderer = Renderer::new(ctx.ns_view(), scale).expect("renderer init");
        self.renderer = Some(renderer);
        // Initial size will arrive via the explicit Resized fired by run_app.
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        if self.session.pump() > 0 {
            ctx.request_redraw();
        }
        if self.session.is_exited() {
            self.session.pump(); // commit final bytes
            ctx.exit();
        }
    }

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, modifiers: Modifiers) {
        if let Some(bytes) = key_event_to_bytes(&event, modifiers) {
            if self.view_offset != 0 {
                self.view_offset = 0;
                ctx.request_redraw();
            }
            let _ = self.session.write(&bytes);
        }
    }

    fn mouse_down(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64) {
        // mcli has no sidebar / no layout — clicks are no-ops for now.
    }

    fn scroll(&mut self, ctx: &MarspotAppCtx, _dx_phys: f64, dy_phys: f64, precise: bool) {
        let cell_h = self
            .renderer
            .as_ref()
            .map(|r| r.cell_dims().1)
            .unwrap_or(15.0);
        // See main.rs comment on the negation: NSEvent positive Y =
        // scroll up; view_offset increases as we look back in
        // scrollback, so negate.
        let lines_f = if precise {
            -dy_phys / cell_h
        } else {
            -dy_phys * 3.0
        };
        if lines_f.abs() < 0.5 {
            return;
        }
        let max = self.session.terminal.grid().scrollback_len() as i32;
        let new = (self.view_offset as i32 + lines_f as i32).clamp(0, max) as u16;
        if new != self.view_offset {
            self.view_offset = new;
            ctx.request_redraw();
        }
    }

    fn resized(&mut self, _ctx: &MarspotAppCtx, phys_w: f64, phys_h: f64) {
        let Some(r) = self.renderer.as_mut() else { return };
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
            title: "",
            selection: None,
        };
        r.render(view);
    }

    fn focused(&mut self, ctx: &MarspotAppCtx, focused: bool) {
        if let Some(r) = self.renderer.as_mut() {
            r.set_window_focused(focused);
            ctx.request_redraw();
        }
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        ctx.exit();
    }

    fn redraw(&mut self, _ctx: &MarspotAppCtx) {
        let Some(r) = self.renderer.as_mut() else { return };
        let view = SessionView {
            grid: self.session.terminal.grid(),
            view_offset: self.view_offset,
            cursor_visible: self.session.terminal.cursor_visible(),
            focused: true,
            title: "",
            selection: None,
        };
        r.render(view);
    }
}

fn main() {
    let proxy = EventProxy::new();
    let proxy_clone = proxy.clone();
    let wake = move || {
        proxy_clone.wake();
    };

    let session =
        Session::spawn(INITIAL_COLS, INITIAL_ROWS, wake).expect("spawn initial session");

    let app = Mcli {
        renderer: None,
        session,
        view_offset: 0,
    };

    let attrs = WindowAttrs {
        title: format!(
            "mcli — {} {}",
            env!("CARGO_PKG_VERSION"),
            env!("MARSPOT_GIT_SHA"),
        ),
        width_logical: 960.0,
        height_logical: 600.0,
    };
    run_app(app, proxy, attrs);
}

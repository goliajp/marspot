//! mcli — minimal standalone single-session terminal.
//!
//! Holds one `Pane` (from `marspot::pane`). All single-session
//! behaviour — key encoding, scroll-into-scrollback, resize, render —
//! lives on `Pane` in the library; this file just dispatches AppKit
//! callbacks to it. That's the steel-cement-stone separation: any
//! single-session feature added to `Pane` is automatically picked up
//! by marspot's N panes too, so mcli is the smallest possible
//! reference implementation of the single-pane terminal — fast,
//! polish-friendly, and a credible standalone product.

use objc2_app_kit::NSScreen;
use objc2_foundation::MainThreadMarker;

use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::pane::Pane;
use marspot::render_metal::{MetalRenderer, WindowRender};
use marspot::session::Session;
use marspot::HEADER_PT;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;

struct Mcli {
    renderer: Option<MetalRenderer>,
    /// mcli is single-window by construction; one of these is all it
    /// will ever need.
    window_render: WindowRender,
    pane: Pane,
}

impl MarspotApp for Mcli {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        // objc2_foundation 0.2.2's NSArray bindings mis-encode NSScreen
        // array's count selector ('q' vs 'Q'), so iterating `screens()`
        // panics at runtime in some launch contexts. main.rs sidesteps
        // it by using the single-screen mainScreen API; do the same
        // here — for default scale derivation the main screen is fine.
        let mt = MainThreadMarker::new().expect("main thread");
        let max_scale = NSScreen::mainScreen(mt)
            .map(|s| s.backingScaleFactor() as f32)
            .unwrap_or(1.0);
        let scale: f32 = std::env::var("MARSPOT_SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(max_scale);

        let mut renderer = MetalRenderer::new(ctx.ns_view(), scale).expect("metal renderer init");
        // Reserve the same top chrome strip marspot does so the macOS
        // traffic-light buttons don't paint over the grid's first row.
        renderer.set_top_inset(HEADER_PT * scale as f64);
        self.renderer = Some(renderer);
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        if self.pane.pump() > 0 {
            ctx.request_redraw();
        }
        if self.pane.is_exited() {
            self.pane.pump(); // commit final bytes
            ctx.exit();
        }
    }

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, modifiers: Modifiers) {
        if self.pane.handle_key(&event, modifiers) {
            ctx.request_redraw();
        }
    }

    fn mouse_down(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64, _modifiers: marspot::input::Modifiers) {
        // mcli has no sidebar / no layout — clicks are no-ops for now.
    }

    fn scroll(&mut self, ctx: &MarspotAppCtx, _dx_phys: f64, dy_phys: f64, precise: bool) {
        let cell_h = self
            .renderer
            .as_ref()
            .map(|r| r.cell_dims().1)
            .unwrap_or(15.0);
        // Natural-scroll-on macOS sends positive dy_phys when the
        // user wants to look UP into scrollback → view_offset should
        // INCREASE in the same direction. No negation. Matches
        // marspot's scroll mapping.
        let lines_f = if precise { dy_phys / cell_h } else { dy_phys * 3.0 };
        if lines_f.abs() < 0.5 {
            return;
        }
        if self.pane.apply_scroll_lines(lines_f as i32) {
            ctx.request_redraw();
        }
    }

    fn resized(&mut self, _ctx: &MarspotAppCtx, phys_w: f64, phys_h: f64) {
        let Some(r) = self.renderer.as_mut() else { return };
        r.resize(phys_w, phys_h);
        let (cell_w, cell_h) = r.cell_dims();
        // Subtract the chrome strip so the session doesn't request rows
        // that would render under the traffic lights.
        let usable_h = (phys_h - r.top_inset_phys()).max(0.0);
        let cols = ((phys_w / cell_w).floor() as u16).max(1);
        let rows = ((usable_h / cell_h).floor() as u16).max(1);
        self.pane.resize(cols, rows);
        r.render(&mut self.window_render, self.pane.view(true, "", "", false, ""));
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

    fn redraw(&mut self, ctx: &MarspotAppCtx) {
        let Some(r) = self.renderer.as_mut() else { return };
        let view = self.pane.view(true, "", "", false, "");
        let caret = r.focused_caret_view_phys_rect(&view);
        r.render(&mut self.window_render, view);
        ctx.set_caret_rect_phys(caret);
    }
}

fn main() {
    // Build identity stamp — print to stderr so a quick log check
    // confirms "yes this is the binary I just built".
    eprintln!(
        "[mcli] version={} git={} built={}",
        env!("CARGO_PKG_VERSION"),
        env!("MARSPOT_GIT_SHA"),
        env!("MARSPOT_BUILD_TS"),
    );

    let proxy = EventProxy::new();
    let proxy_clone = proxy.clone();
    let wake = move || {
        proxy_clone.wake();
    };

    let session =
        Session::spawn(INITIAL_COLS, INITIAL_ROWS, wake).expect("spawn initial session");

    let app = Mcli {
        renderer: None,
        window_render: WindowRender::new(),
        pane: Pane::new(session),
    };

    let attrs = WindowAttrs {
        title: format!(
            "mcli — {} {} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("MARSPOT_GIT_SHA"),
            env!("MARSPOT_BUILD_TS"),
        ),
        width_logical: 960.0,
        height_logical: 600.0,
        frame_pt: None,
        bg: (0.022, 0.028, 0.042), // chrome panel tone (BG_PANEL)
    };
    run_app(app, proxy, attrs);
}

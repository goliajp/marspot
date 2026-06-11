//! marspot-shell — the outer process.
//!
//! Owns the NSWindow + NSApp event loop and a CAMetalLayer that
//! displays one IOSurface.  Spawns `marspot-coreshim` (Step 1) /
//! `marspot-core` (Step 2+) as a child, hands it the IOSurface ID via
//! environment variables, and presents whatever the child writes.
//!
//! Designed to be *boring* — anything substantive (parser, renderer,
//! input handling, layout) lives in the core process, behind a binary
//! we hot-swap during silent updates.  Keeping shell logic minimal
//! and dependencies thin means the shell almost never has to update,
//! which is what makes "no window flicker on upgrade" reachable.
//!
//! Step 1 scope: bring up the IOSurface link with a stub child that
//! draws a rotating gradient — proves the cross-process render path
//! before touching the real renderer.

use std::process::{Child, Command};
use std::time::Duration;

use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::Modifiers;
use marspot::iosurface::IOSurface;
use marspot::shell_proto::{
    ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH,
};

mod present;
use present::ShellPresenter;

const DEFAULT_TITLE: &str = "Marspot";
const DEFAULT_W_PT: f64 = 1200.0;
const DEFAULT_H_PT: f64 = 800.0;
const REDRAW_INTERVAL_MS: u64 = 16; // ~60 fps

struct ShellApp {
    proxy: EventProxy,
    surface: Option<IOSurface>,
    presenter: Option<ShellPresenter>,
    core_child: Option<Child>,
    redraw_thread_started: bool,
}

impl ShellApp {
    fn new(proxy: EventProxy) -> Self {
        Self {
            proxy,
            surface: None,
            presenter: None,
            core_child: None,
            redraw_thread_started: false,
        }
    }

    fn spawn_core(&mut self, surface_id: u32, w_phys: usize, h_phys: usize, scale: f64) {
        // Look for the core binary next to ourselves.  Step 1 uses the
        // coreshim stub; Step 2 will switch to `marspot-core`.
        let core_name =
            std::env::var("MARSPOT_CORE_BIN").unwrap_or_else(|_| "marspot-coreshim".to_string());
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[shell] current_exe failed: {e}");
                return;
            }
        };
        let core_bin = exe
            .parent()
            .map(|p| p.join(&core_name))
            .unwrap_or_else(|| std::path::PathBuf::from(&core_name));

        eprintln!(
            "[shell] spawning core: {} surface_id={surface_id} w={w_phys} h={h_phys} scale={scale}",
            core_bin.display()
        );
        match Command::new(&core_bin)
            .env(ENV_SURFACE_ID, surface_id.to_string())
            .env(ENV_SURFACE_WIDTH, w_phys.to_string())
            .env(ENV_SURFACE_HEIGHT, h_phys.to_string())
            .env(ENV_SURFACE_SCALE, scale.to_string())
            .spawn()
        {
            Ok(child) => {
                eprintln!("[shell] core pid={}", child.id());
                self.core_child = Some(child);
            }
            Err(e) => {
                eprintln!("[shell] spawn core failed: {e}");
            }
        }
    }

    fn start_redraw_pump(&mut self) {
        if self.redraw_thread_started {
            return;
        }
        let proxy = self.proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(REDRAW_INTERVAL_MS));
            proxy.wake();
        });
        self.redraw_thread_started = true;
    }
}

impl MarspotApp for ShellApp {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;

        let surface = match IOSurface::create(w_px, h_px) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[shell] IOSurface::create({w_px}, {h_px}) failed: {e}");
                ctx.exit();
                return;
            }
        };
        surface.increment_use();

        let presenter = match ShellPresenter::new(ctx.ns_view(), scale as f32, &surface) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[shell] ShellPresenter::new failed: {e}");
                ctx.exit();
                return;
            }
        };

        let id = surface.id();
        self.surface = Some(surface);
        self.presenter = Some(presenter);

        self.spawn_core(id, w_px, h_px, scale);
        self.start_redraw_pump();
        ctx.request_redraw();
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        ctx.request_redraw();
    }

    fn key_event(&mut self, _ctx: &MarspotAppCtx, _event: marspot::input::MarspotKeyEvent, _mods: Modifiers) {
        // Step 3: forward to core via control socket.
    }

    fn mouse_down(&mut self, _ctx: &MarspotAppCtx, _x: f64, _y: f64, _mods: Modifiers) {}

    fn scroll(&mut self, _ctx: &MarspotAppCtx, _dx: f64, _dy: f64, _precise: bool) {}

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // Step 4 (resize negotiation) will rebuild the IOSurface and
        // hand the new ID to the core.  For Step 1 we just rebuild the
        // layer drawable size — the IOSurface keeps its original size,
        // which means the quad scales over the new drawable.  Looks
        // stretched on resize until Step 4; intentional, not pretty.
        if let Some(p) = self.presenter.as_mut() {
            p.set_drawable_size(w_phys, h_phys);
        }
        ctx.request_redraw();
    }

    fn focused(&mut self, _ctx: &MarspotAppCtx, _focused: bool) {}

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        if let Some(mut child) = self.core_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(surface) = self.surface.take() {
            surface.decrement_use();
        }
        ctx.exit();
    }

    fn redraw(&mut self, _ctx: &MarspotAppCtx) {
        if let Some(p) = self.presenter.as_mut() {
            p.present();
        }
    }
}

fn main() {
    eprintln!(
        "marspot-shell {} (git {} built {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("MARSPOT_GIT_SHA").unwrap_or("unknown"),
        option_env!("MARSPOT_BUILD_TS").unwrap_or("unknown")
    );
    let attrs = WindowAttrs {
        title: DEFAULT_TITLE.to_string(),
        width_logical: DEFAULT_W_PT,
        height_logical: DEFAULT_H_PT,
    };
    let proxy = EventProxy::new();
    let app = ShellApp::new(proxy.clone());
    run_app(app, proxy, attrs);
}

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

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;

use marspot::app::{run_app, EventProxy, MarspotApp, MarspotAppCtx, WindowAttrs};
use marspot::input::{MarspotKeyEvent, Modifiers};
use marspot::iosurface::IOSurface;
use marspot::shell_proto::{
    encode_focus, encode_key_event, encode_mouse, encode_preedit, encode_resize, encode_scroll,
    event_to_wire, struct_to_mods_byte, Frame, MsgType, DEFAULT_CONTROL_FD, ENV_CONTROL_FD,
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
    /// Parent end of the AF_UNIX socketpair we share with the core.
    /// Wrapped in `Mutex` so the `MarspotApp` callbacks (all on the
    /// main thread, but the type system doesn't know that) can mutate
    /// it without splitting the struct.  Frames written here arrive
    /// at the core's stdin-side fd 3 / `MARSPOT_SHELL_CONTROL_FD`.
    control_tx: Option<Mutex<UnixStream>>,
    redraw_thread_started: bool,
}

impl ShellApp {
    fn new(proxy: EventProxy) -> Self {
        Self {
            proxy,
            surface: None,
            presenter: None,
            core_child: None,
            control_tx: None,
            redraw_thread_started: false,
        }
    }

    /// Send a frame to the core.  Logs and drops on EPIPE; the core
    /// dying mid-session is handled by the supervisor (Step 5+), so
    /// here we just don't crash the shell.
    fn send(&self, msg_type: MsgType, payload: Vec<u8>) {
        let Some(tx) = self.control_tx.as_ref() else {
            return;
        };
        let frame = Frame::new(msg_type, payload);
        let mut stream = match tx.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned: still try
        };
        if let Err(e) = frame.write_to(&mut *stream) {
            eprintln!("[shell] control frame {:?} write failed: {e}", msg_type);
        }
    }

    fn spawn_core(&mut self, surface_id: u32, w_phys: usize, h_phys: usize, scale: f64) {
        // Look for the core binary next to ourselves.  Default is the
        // real `marspot-core` (Step 2+); override with MARSPOT_CORE_BIN
        // to point at `marspot-coreshim` for IOSurface-link bring-up
        // tests.
        let core_name =
            std::env::var("MARSPOT_CORE_BIN").unwrap_or_else(|_| "marspot-core".to_string());
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

        // Create the bidirectional control socket BEFORE spawn so the
        // child can inherit one end as fd 3.  socketpair(AF_UNIX,
        // SOCK_STREAM) gives us two fds in the parent: parent_fd keeps
        // the shell's side; child_fd gets dup2'd to 3 in pre_exec, then
        // closed in the parent after spawn returns.
        let mut sp = [0i32; 2];
        let r = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr())
        };
        if r != 0 {
            eprintln!(
                "[shell] socketpair failed: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        let parent_fd: RawFd = sp[0];
        let child_fd: RawFd = sp[1];

        eprintln!(
            "[shell] spawning core: {} surface_id={surface_id} w={w_phys} h={h_phys} scale={scale} control_fd={DEFAULT_CONTROL_FD}",
            core_bin.display()
        );
        let mut cmd = Command::new(&core_bin);
        cmd.env(ENV_SURFACE_ID, surface_id.to_string())
            .env(ENV_SURFACE_WIDTH, w_phys.to_string())
            .env(ENV_SURFACE_HEIGHT, h_phys.to_string())
            .env(ENV_SURFACE_SCALE, scale.to_string())
            .env(ENV_CONTROL_FD, DEFAULT_CONTROL_FD.to_string());
        // SAFETY: pre_exec runs in the forked child between fork and
        // exec.  Only async-signal-safe libc calls are allowed; we
        // only use dup2/close/fcntl which are all on the AS-safe list.
        unsafe {
            cmd.pre_exec(move || {
                if child_fd != DEFAULT_CONTROL_FD {
                    if libc::dup2(child_fd, DEFAULT_CONTROL_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(child_fd);
                }
                // Strip CLOEXEC so the core sees fd 3 after exec.
                let flags = libc::fcntl(DEFAULT_CONTROL_FD, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(
                        DEFAULT_CONTROL_FD,
                        libc::F_SETFD,
                        flags & !libc::FD_CLOEXEC,
                    );
                }
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                eprintln!("[shell] core pid={}", child.id());
                self.core_child = Some(child);
                // Parent no longer needs the child end.
                unsafe { libc::close(child_fd) };
                // Wrap the parent end as a UnixStream we can write
                // frames to from any callback.
                let stream = unsafe { UnixStream::from_raw_fd(parent_fd) };
                self.control_tx = Some(Mutex::new(stream));
            }
            Err(e) => {
                eprintln!("[shell] spawn core failed: {e}");
                unsafe {
                    libc::close(parent_fd);
                    libc::close(child_fd);
                }
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

    fn key_event(&mut self, _ctx: &MarspotAppCtx, event: MarspotKeyEvent, mods: Modifiers) {
        let wire = event_to_wire(&event, mods);
        self.send(MsgType::KeyEvent, encode_key_event(&wire));
    }

    fn mouse_down(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64, mods: Modifiers) {
        self.send(
            MsgType::MouseDown,
            encode_mouse(x, y, struct_to_mods_byte(mods)),
        );
    }

    fn mouse_drag(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64) {
        // No modifier info on drag — pass zero; the renderer doesn't
        // currently need mods for drag-extend selection.
        self.send(MsgType::MouseDrag, encode_mouse(x, y, 0));
    }

    fn mouse_up(&mut self, _ctx: &MarspotAppCtx, x: f64, y: f64) {
        self.send(MsgType::MouseUp, encode_mouse(x, y, 0));
    }

    fn scroll(&mut self, _ctx: &MarspotAppCtx, dx: f64, dy: f64, precise: bool) {
        self.send(MsgType::Scroll, encode_scroll(dx, dy, precise));
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // Step 4 (resize negotiation) will rebuild the IOSurface and
        // hand the new ID to the core.  For now we tell the core the
        // new dimensions over the control socket — it can update its
        // layout math — and rebuild the layer drawable size.  The
        // surface itself doesn't grow until the full handoff lands.
        if let Some(p) = self.presenter.as_mut() {
            p.set_drawable_size(w_phys, h_phys);
        }
        let scale = ctx.scale();
        self.send(MsgType::Resize, encode_resize(w_phys, h_phys, scale));
        ctx.request_redraw();
    }

    fn focused(&mut self, _ctx: &MarspotAppCtx, focused: bool) {
        self.send(MsgType::Focus, encode_focus(focused));
    }

    fn ime_preedit_changed(&mut self, _ctx: &MarspotAppCtx, text: &str) {
        self.send(MsgType::Preedit, encode_preedit(text));
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        // Drop control socket first — gives the core a clean EOF on
        // its read side so it can shut down gracefully before SIGKILL.
        self.control_tx = None;
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

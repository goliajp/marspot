//! `DevWindow` — independent NSWindow for the UI-system dev panel.
//!
//! Architectural choice (Plan B from the 2026-06-22 conversation):
//! the dev panel is an **independent macOS window**, not an
//! overlay rendered into the main marspot window.  User can drag
//! it onto a second monitor, off-screen for screenshots, etc.
//!
//! ## What lives here
//!
//! `DevWindow` is a self-contained subsystem:
//! - its own `NSWindow` (titled, draggable, closable)
//! - its own `NSView`
//! - its own `CAMetalLayer` attached to that view
//! - its own `MetalRenderer` so device / queue / pipelines /
//!   atlas are not shared with the main window (clean ownership;
//!   the cost is ~5 MiB of duplicate atlas plus the device's
//!   one-time pipeline-state-object construction)
//!
//! ## What this commit deliberately skips
//!
//! - **Mouse / scroll routing.**  Title bar drag works because
//!   it's pure AppKit; clicks inside the content area don't yet
//!   dispatch.
//! - **Close button → main app state sync.**  Clicking the red
//!   X currently just hides via AppKit's default close path;
//!   the toolbar toggle in the main window won't observe it.
//! - **Persistence.**  Window position + size resets per launch.
//!
//! Those land in subsequent commits.  This file establishes the
//! ownership shape + the call sites in `MarspotApp::resumed` and
//! `MarspotApp::redraw` first.

use std::cell::RefCell;

use objc2::declare_class;
use objc2::msg_send_id;
use objc2::mutability;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::ClassType;
use objc2::DeclaredClass;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::render_metal::MetalRenderer;
use crate::ui::components::DevPanelState;

// ─── NSWindowDelegate ─────────────────────────────────────────
// Routes the dev window's resize / move / focus notifications to
// the host's redraw cycle.

struct DevWindowDelegateIvars;

declare_class!(
    struct DevWindowDelegate;

    unsafe impl ClassType for DevWindowDelegate {
        type Super = objc2::runtime::NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "MarspotDevWindowDelegate";
    }

    impl DeclaredClass for DevWindowDelegate {
        type Ivars = DevWindowDelegateIvars;
    }

    unsafe impl NSObjectProtocol for DevWindowDelegate {}

    unsafe impl NSWindowDelegate for DevWindowDelegate {
        #[method(windowDidResize:)]
        fn window_did_resize(&self, _n: &NSNotification) {
            crate::app::dispatch_event_pub(crate::app::EventKind::DevWindowChanged);
        }

        #[method(windowDidMove:)]
        fn window_did_move(&self, _n: &NSNotification) {
            crate::app::dispatch_event_pub(crate::app::EventKind::DevWindowChanged);
        }

        #[method(windowDidChangeBackingProperties:)]
        fn window_did_change_backing(&self, _n: &NSNotification) {
            // User dragged the window across displays of different
            // DPI.  Re-render so the next frame picks up the new
            // backingScaleFactor.
            crate::app::dispatch_event_pub(crate::app::EventKind::DevWindowChanged);
        }
    }
);

/// Marker so we don't repeat-build (NSWindow construction must be
/// idempotent — first build wins, subsequent shows just orderFront).
thread_local! {
    static DEV_WINDOW_HANDLE: RefCell<Option<DevWindow>> = const { RefCell::new(None) };
    /// Hold the delegate Retained so NSWindow's weak ref stays
    /// alive for the window's whole lifetime.
    static DEV_WINDOW_DELEGATE: RefCell<Option<Retained<DevWindowDelegate>>> = const { RefCell::new(None) };
    /// Deferred visibility request.  `set_visible_deferred` writes
    /// here; the host calls `drain_pending_actions` AFTER its
    /// `APP_STATE` borrow drops, then actual `makeKeyAndOrderFront`
    /// / `orderOut` runs without risking a re-entrant
    /// `borrow_mut` panic when AppKit synchronously fires
    /// `windowDidBecomeKey` / etc. into our window-delegate path.
    static PENDING_VISIBLE: RefCell<Option<bool>> = const { RefCell::new(None) };
}

pub struct DevWindow {
    nswindow: Retained<NSWindow>,
    view: Retained<NSView>,
    renderer: MetalRenderer,
    /// Logical-pt frame of the dev window the last time render
    /// observed it.  Re-used as the rectangle Canvas resolves
    /// against (the dev panel is full-window-bounds inside its
    /// own NSWindow).
    last_frame_pt: (f64, f64, f64, f64),
}

impl DevWindow {
    /// Build the dev window.  Sized for a comfortable workbench
    /// (the user can drag-resize from the corners via standard
    /// NSWindow chrome).  Hidden on construction — caller
    /// `set_visible(true)` to bring it onscreen.
    pub fn build(mtm: MainThreadMarker) -> Result<Self, String> {
        // ── 1. NSView, sized to the initial logical frame. ──
        // The dev window's content is fully managed by Canvas;
        // a vanilla NSView (not MarspotView) is enough — no IME
        // surface, no key event routing in this commit.
        let initial_w = 420.0_f64;
        let initial_h = 520.0_f64;
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(initial_w, initial_h),
        );
        let view: Retained<NSView> = unsafe {
            let alloc = mtm.alloc::<NSView>();
            msg_send_id![alloc, initWithFrame: frame]
        };
        // Autoresize so the view always matches the NSWindow's
        // content area as the user drag-resizes the window.
        // Without this the view stays pinned at 420×520 and the
        // panel canvas only paints the top-left corner of a
        // grown window.
        unsafe {
            use objc2_app_kit::NSAutoresizingMaskOptions;
            view.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable
                    | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
        }

        // ── 2. NSWindow ──
        // Titled + Closable + Resizable so the user can drag and
        // close via standard macOS chrome.  No FullSizeContentView:
        // we WANT the system title bar visible so the drag affordance
        // is unambiguous.
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let nswindow: Retained<NSWindow> = unsafe {
            let alloc = mtm.alloc::<NSWindow>();
            msg_send_id![alloc,
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::NSBackingStoreBuffered,
                defer: false
            ]
        };
        nswindow.setTitle(&NSString::from_str("UI Dev Panel"));
        // BG matches the panel's own BG so the title bar reads as
        // one continuous surface with the content beneath it.
        unsafe {
            let bg = NSColor::colorWithSRGBRed_green_blue_alpha(
                20.0 / 255.0, 22.0 / 255.0, 28.0 / 255.0, 1.0,
            );
            nswindow.setBackgroundColor(Some(&bg));
        }
        nswindow.setContentView(Some(&view));
        // Float above main window when both are visible (until the
        // user explicitly orders main to front).  Comments below
        // explain the level choice — we want "above normal docs"
        // without becoming "always-on-top of all apps."
        unsafe {
            use objc2_app_kit::NSWindowLevel;
            // NSFloatingWindowLevel = floating panel, above normal.
            const NS_FLOATING_LEVEL: NSWindowLevel = 3;
            nswindow.setLevel(NS_FLOATING_LEVEL);
        }

        // ── 3. MetalRenderer ── attaches a CAMetalLayer to the
        // view.  Same code path the main window uses.  Scale is
        // the device pixel ratio at construction; later commits
        // will track the window's backingScaleFactor live.
        let scale = unsafe {
            nswindow.screen()
                .map(|s| s.backingScaleFactor() as f32)
                .unwrap_or(2.0)
        };
        let renderer = MetalRenderer::new(&view, scale)
            .map_err(|e| format!("dev window renderer init: {e}"))?;

        // Attach a delegate so resize / move / display-change
        // notifications drive a host redraw.
        let delegate: Retained<DevWindowDelegate> = {
            let alloc = mtm.alloc::<DevWindowDelegate>().set_ivars(DevWindowDelegateIvars);
            unsafe { msg_send_id![super(alloc), init] }
        };
        let proto: &ProtocolObject<dyn NSWindowDelegate> =
            ProtocolObject::from_ref(&*delegate);
        nswindow.setDelegate(Some(proto));
        // Hold the delegate alive for the window's lifetime —
        // NSWindow holds it weakly.  We stash in a TLS slot.
        DEV_WINDOW_DELEGATE.with(|c| *c.borrow_mut() = Some(delegate));

        Ok(Self {
            nswindow,
            view,
            renderer,
            last_frame_pt: (0.0, 0.0, initial_w, initial_h),
        })
    }

    /// Direct-call AppKit show/hide.  ONLY safe outside the
    /// host's `APP_STATE` borrow — `makeKeyAndOrderFront:` /
    /// `orderOut:` synchronously fire window-delegate
    /// notifications that re-enter Rust via `dispatch_event`.
    /// Inside redraw / event-handler closures, use
    /// [`DevWindow::set_visible_deferred`] instead and let the
    /// host's drain loop apply the change.
    pub fn set_visible_now(&self, visible: bool) {
        if visible == self.is_visible() {
            return;
        }
        if visible {
            self.nswindow.makeKeyAndOrderFront(None);
        } else {
            self.nswindow.orderOut(None);
        }
    }

    /// Queue a visibility change for the host's next drain pass.
    /// Idempotent: later calls overwrite earlier ones (the host
    /// will apply only the final value).
    pub fn set_visible_deferred(&self, visible: bool) {
        let _ = self; // method signature parity with set_visible_now
        PENDING_VISIBLE.with(|cell| *cell.borrow_mut() = Some(visible));
    }

    /// True when the underlying NSWindow is onscreen.
    pub fn is_visible(&self) -> bool {
        self.nswindow.isVisible()
    }

    /// Logical-pt frame of the dev window (x, y, w, h).
    /// Persists for later commits; not yet read by the renderer.
    pub fn frame_pt(&self) -> (f64, f64, f64, f64) {
        let f = self.nswindow.frame();
        (f.origin.x, f.origin.y, f.size.width, f.size.height)
    }

    /// Render the dev panel state into our own Metal layer.  No-op
    /// when the window isn't visible (AppKit hands us drawables
    /// even for hidden layers, but doing the work is wasted).
    pub fn render(&mut self, state: &DevPanelState) {
        if !self.is_visible() {
            return;
        }
        let scale = unsafe {
            self.nswindow.screen()
                .map(|s| s.backingScaleFactor() as f64)
                .unwrap_or(2.0)
        };
        // Pull the actual content-area size off the NSWindow each
        // frame.  Autoresizing on the content view is unreliable in
        // some macOS configs (especially when the window starts
        // small then is dragged large); doing the sync here makes
        // the view + layer always match what the user sees.
        let content_rect = unsafe {
            use objc2_foundation::CGRect;
            let frame = self.nswindow.frame();
            let style = self.nswindow.styleMask();
            let cr: CGRect = objc2::msg_send![
                &*self.nswindow,
                contentRectForFrameRect: frame,
                styleMask: style
            ];
            cr
        };
        let w_pt = content_rect.size.width;
        let h_pt = content_rect.size.height;
        // Sync the view's frame so subsequent `bounds()` calls and
        // hit-tests see the right size.
        use objc2_foundation::CGSize;
        unsafe {
            self.view.setFrameSize(CGSize { width: w_pt, height: h_pt });
        }
        let width_phys = w_pt * scale;
        let height_phys = h_pt * scale;
        self.last_frame_pt = (
            content_rect.origin.x, content_rect.origin.y,
            w_pt, h_pt,
        );

        // Build a Canvas for the dev panel content.  At this point
        // the panel's `origin_pt` field is meaningless (it's a
        // window-relative position, but our window is exactly the
        // panel surface).  Override to (0, 0) + window size so the
        // existing build_dev_panel_canvas paints filling the window.
        let mut s = state.clone();
        s.scale = scale;
        s.origin_pt = (0.0, 0.0);
        s.size_pt = (w_pt, h_pt);
        s.visible = true;

        // Reuse the renderer's standalone canvas path.  The dev
        // window doesn't have a `Layout` or sessions, so we can't
        // call `render_layout`.  Instead use `render_canvas` to
        // paint just our one Canvas.
        let chrome_cell_w = self.renderer_font_metrics().0 as f32;
        let chrome_cell_h = self.renderer_font_metrics().1 as f32;
        let chrome_ascent = self.renderer_font_metrics().2 as f32;
        let canvas = crate::ui::components::build_dev_panel_canvas(
            &s, width_phys, height_phys,
            chrome_cell_w, chrome_cell_h,
        );
        self.renderer.render_canvas_into_layer(
            &canvas,
            width_phys as f32,
            height_phys as f32,
            chrome_cell_w, chrome_cell_h, chrome_ascent,
        );
    }

    fn renderer_font_metrics(&self) -> (f64, f64, f64) {
        let f = self.renderer.chrome_font_metrics();
        (f.0 as f64, f.1 as f64, f.2 as f64)
    }
}

/// Build the dev window if it doesn't exist yet and stash it.
/// Idempotent across calls; main app invokes this once during
/// `resumed`.
pub fn ensure_built(mtm: MainThreadMarker) -> Result<(), String> {
    DEV_WINDOW_HANDLE.with(|cell| {
        if cell.borrow().is_some() {
            return Ok(());
        }
        let w = DevWindow::build(mtm)?;
        *cell.borrow_mut() = Some(w);
        Ok(())
    })
}

/// Mutate the dev window via a closure.  Returns `None` when the
/// window hasn't been built yet (early in app startup).
pub fn with_dev_window<R>(f: impl FnOnce(&mut DevWindow) -> R) -> Option<R> {
    DEV_WINDOW_HANDLE.with(|cell| {
        cell.borrow_mut().as_mut().map(f)
    })
}

/// Apply any deferred visibility request queued via
/// [`DevWindow::set_visible_deferred`].  The host (`app.rs`'s
/// `dispatch_event`) calls this AFTER its `APP_STATE` borrow
/// drops, so re-entrant `dispatch_event` calls from AppKit
/// notifications can run safely.  No-op when no request is
/// queued.
///
/// **Critical** — the AppKit calls (`makeKeyAndOrderFront:` /
/// `orderOut:`) are made AFTER the DEV_WINDOW_HANDLE borrow is
/// dropped.  Without that, the synchronous `windowDidMove` /
/// `windowDidBecomeKey` notifications AppKit fires re-enter the
/// host's `dispatch_event` → `with_dev_window` → second
/// `borrow_mut` → panic.  We clone the `Retained<NSWindow>` out
/// (cheap refcount bump), drop the cell borrow, then act.
pub fn drain_pending_actions() {
    let pending = PENDING_VISIBLE.with(|cell| cell.borrow_mut().take());
    if let Some(visible) = pending {
        let nswindow = DEV_WINDOW_HANDLE.with(|cell| {
            cell.borrow().as_ref().map(|w| w.nswindow.clone())
        });
        if let Some(window) = nswindow {
            let already_visible = window.isVisible();
            if visible != already_visible {
                if visible {
                    window.makeKeyAndOrderFront(None);
                } else {
                    window.orderOut(None);
                }
            }
        }
    }
}

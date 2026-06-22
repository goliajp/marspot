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
    NSBackingStoreType, NSColor, NSEvent, NSView, NSWindow, NSWindowDelegate,
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

// ─── Custom NSView with mouseDown routing ─────────────────────
// Without this, AppKit swallows clicks inside the dev window's
// content area — the user can drag the title bar but the menu /
// tab strip in our canvas are inert.  DevPanelView captures the
// mouseDown event, converts to logical pt in view-local coords
// (isFlipped=true so y=0 is at top, matching Canvas), and
// dispatches `EventKind::DevPanelClick`.

struct DevPanelViewIvars;

declare_class!(
    struct DevPanelView;

    unsafe impl ClassType for DevPanelView {
        #[inherits(objc2_app_kit::NSResponder, objc2::runtime::NSObject)]
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "MarspotDevPanelView";
    }

    impl DeclaredClass for DevPanelView {
        type Ivars = DevPanelViewIvars;
    }

    unsafe impl NSObjectProtocol for DevPanelView {}

    unsafe impl DevPanelView {
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool { true }

        // First click on a non-key window lands AS a click (vs just
        // activating the window).  Without this the user has to click
        // twice when switching from main marspot window to dev panel.
        #[method(acceptsFirstMouse:)]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool { true }

        // y=0 at top, matching Canvas's coord convention.
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool { true }

        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            let loc_window = unsafe { event.locationInWindow() };
            // convertPoint:fromView:nil = window coords → view coords;
            // with isFlipped=true this yields top-down y in logical pt.
            let loc_view = self.convertPoint_fromView(loc_window, None);
            // Canvas / dev panel layout are in logical pt — pass them
            // straight through, no scaling.  L1's `dev_panel_click`
            // hit-tests against the same `Pt` constants.
            crate::app::dispatch_event_pub(
                crate::app::EventKind::DevPanelClick {
                    x_pt: loc_view.x,
                    y_pt: loc_view.y,
                },
            );
        }

        // Scroll wheel — route to whichever ScrollView contains the
        // pointer.  v1 has a single ScrollView (the Model section),
        // so we just apply the delta to it; future multi-scroll-area
        // dev panel will hit-test for which scroll target.  delta_y
        // sign: AppKit "scroll up" event reports +y on a flipped
        // view, but for scroll-content semantics "scroll up" =
        // content moves DOWN = offset_y DECREASES.  So we apply -y.
        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            let dy = unsafe { event.scrollingDeltaY() };
            // Approximate "logical-pt × scale = phys pixels" — AppKit
            // scrollingDeltaY is already in points (typically magnitudes
            // like 1.0–20.0 on Apple trackpads).  Layout state is in
            // phys, so multiply by 2 (default Retina scale) — actual
            // scale comes through dispatch.
            let _ = dy;
            crate::app::dispatch_event_pub(
                crate::app::EventKind::DevPanelScroll {
                    delta_y_pt: -dy,  // invert: trackpad up = content up = offset down
                },
            );
        }

        // Trackpad ought to feel snappy — accept first responder so
        // wheel reaches us without an extra click.
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
    /// Same deferral pattern for `setFrame:display:` — calling it
    /// inline from `resumed` (which holds the APP_STATE borrow)
    /// crashes the process because the synchronous `windowDidMove:`
    /// delegate notification re-enters `dispatch_event` and tries
    /// to `borrow_mut` an already-borrowed cell.  See the
    /// 2026-06-23 incident (panic in `DevWindowDelegate::window_did_move`,
    /// process aborts because objc2 declare_class methods are nounwind).
    static PENDING_FRAME: RefCell<Option<(f64, f64, f64, f64)>> = const { RefCell::new(None) };
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
        // Custom NSView subclass so mouseDown / acceptsFirstMouse
        // wire up — vanilla NSView swallows clicks.  Keep the
        // subclass type alive in `dpv` and use a raw-ptr upcast
        // when handing to NSWindow / storing in the struct's
        // `Retained<NSView>` field (same trick as `app.rs::setContentView`).
        let dpv: Retained<DevPanelView> = {
            let alloc = mtm.alloc::<DevPanelView>().set_ivars(DevPanelViewIvars);
            unsafe { msg_send_id![super(alloc), initWithFrame: frame] }
        };
        let view: Retained<NSView> = unsafe {
            // DevPanelView ⊆ NSView (declared via inherits).  Bump
            // the retain count for the second handle; both refs will
            // release on drop.
            let raw: *const NSView = Retained::as_ptr(&dpv) as *const NSView;
            Retained::retain(raw as *mut NSView).expect("DevPanelView -> NSView")
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

    /// Logical-pt frame of the dev window (x, y, w, h).  Used by L1
    /// to persist the window's geometry to `dev-window-state.bin`.
    pub fn frame_pt(&self) -> (f64, f64, f64, f64) {
        let f = self.nswindow.frame();
        (f.origin.x, f.origin.y, f.size.width, f.size.height)
    }

    /// `CGDirectDisplayID` (u32) of the screen this window is on.
    /// Mirrors `MarspotAppCtx::window_display_id` so the persistence
    /// record round-trips identically to the main window's record.
    pub fn display_id(&self) -> Option<u32> {
        unsafe {
            let screen = self.nswindow.screen()?;
            let desc = screen.deviceDescription();
            use objc2_foundation::{NSNumber, NSString};
            let key = NSString::from_str("NSScreenNumber");
            let val = desc.objectForKey(key.as_ref())?;
            let ptr: *const objc2::runtime::AnyObject = &*val;
            let num: &NSNumber = &*(ptr as *const NSNumber);
            Some(num.unsignedIntValue())
        }
    }

    /// Restore a previously-saved geometry.  Logical pt, screen
    /// coordinates (bottom-left origin per NSWindow).  AppKit silently
    /// clamps to a usable region — off-screen geometry from a
    /// now-disconnected display is dealt with by AppKit, not us.
    ///
    /// **Deferred** — `setFrame:display:` synchronously fires
    /// `windowDidMove:` into our delegate, which dispatches a
    /// `DevWindowChanged` event back through `APP_STATE`.  If the
    /// caller is already inside an `APP_STATE.borrow_mut()` (e.g.,
    /// `MarspotApp::resumed`), the re-entrant borrow panics in a
    /// `nounwind` objc2 method = process abort.  We write to a
    /// `PENDING_FRAME` thread-local instead; `drain_pending_actions`
    /// applies it after the host's borrow drops.  Same pattern as
    /// `set_visible_deferred`.
    pub fn apply_saved_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        if w < 50.0 || h < 50.0 {
            return;
        }
        PENDING_FRAME.with(|cell| *cell.borrow_mut() = Some((x, y, w, h)));
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
            chrome_cell_w, chrome_cell_h, chrome_ascent,
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

    /// Public-API wrapper around the renderer's chrome cell metrics
    /// (physical px).  Used by L1's `dev_panel_click` hit-test to
    /// recover the same logical-pt tab widths the canvas painted.
    pub fn chrome_cell_dims_phys(&self) -> (f64, f64) {
        let f = self.renderer.chrome_font_metrics();
        (f.0 as f64, f.1 as f64)
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
    // PENDING_FRAME goes first — apply the saved geometry before
    // showing the window so the user doesn't see it flash at the
    // default 420×552 origin then jump to the saved position.
    let pending_frame = PENDING_FRAME.with(|cell| cell.borrow_mut().take());
    if let Some((x, y, w, h)) = pending_frame {
        let nswindow = DEV_WINDOW_HANDLE.with(|cell| {
            cell.borrow().as_ref().map(|wh| wh.nswindow.clone())
        });
        if let Some(window) = nswindow {
            let r = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
            unsafe { window.setFrame_display(r, true); }
        }
    }

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

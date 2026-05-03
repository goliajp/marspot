//! Mars's AppKit-direct window + event loop.
//!
//! Replaces winit.  We talk to NSApplication / NSWindow / NSView
//! through `objc2-app-kit` and dispatch events into a `MarsApp` impl.
//!
//! The shape mirrors winit's `ApplicationHandler` (resumed → events
//! → user_event) but on a smaller surface — only the events Mars
//! actually consumes.  Lives in the lib because both `mars` and
//! `mcli` use it.
//!
//! ## Threading
//!
//! Everything runs on the main thread except `EventProxy::wake()`,
//! which can be called from any thread.  Wake signals a
//! CFRunLoopSource registered on the main run loop; on the next
//! main-thread iteration the source's perform fires `MarsApp::user_event`.
//!
//! ## Known regressions vs winit
//!
//! - **No IME / dead-key resolution.**  We pass `NSEvent.characters`
//!   straight through.  Adding `NSTextInputClient` is a follow-up.
//! - **No `CursorMoved` event.**  Mars only inspects the cursor at
//!   click time; we read `locationInWindow` from `mouseDown:` instead.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, OnceLock};

use core_foundation::base::CFIndex;
use core_foundation::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRef,
    CFRunLoopSourceContext, CFRunLoopSourceCreate, CFRunLoopSourceRef, CFRunLoopSourceSignal,
    CFRunLoopWakeUp,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent,
    NSEventModifierFlags, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::input::{KeyState, LogicalKey, MarsKeyEvent, Modifiers, NamedKey};

/// Trait that the binary's main loop implements.  Methods are called
/// on the main thread; the `MarsAppCtx` argument lets handlers
/// request a redraw or schedule exit.
pub trait MarsApp: 'static {
    /// Called once after the window is created and the run loop has
    /// started.  Renderer setup belongs here.
    fn resumed(&mut self, ctx: &MarsAppCtx);

    /// Called whenever `EventProxy::wake()` is signalled from any
    /// thread.  Coalesced — multiple wakes between iterations may
    /// collapse into one call.
    fn user_event(&mut self, ctx: &MarsAppCtx);

    fn key_event(&mut self, ctx: &MarsAppCtx, event: MarsKeyEvent, modifiers: Modifiers);

    /// Mouse-down with location in physical pixels, origin top-left.
    fn mouse_down(&mut self, ctx: &MarsAppCtx, x_phys: f64, y_phys: f64);

    /// Scroll delta in physical pixels (positive Y = scroll content
    /// down).  `precise` is true for trackpad / Magic Mouse, false
    /// for traditional mouse wheels (where deltas come in lines).
    fn scroll(&mut self, ctx: &MarsAppCtx, dx_phys: f64, dy_phys: f64, precise: bool);

    /// Window content size in physical pixels.  Fired on every step
    /// of a live resize plus once after the window is initially shown.
    fn resized(&mut self, ctx: &MarsAppCtx, width_phys: f64, height_phys: f64);

    fn focused(&mut self, ctx: &MarsAppCtx, focused: bool);

    fn close_requested(&mut self, ctx: &MarsAppCtx);

    /// Fired when a previous `ctx.request_redraw()` is being honoured.
    /// Repaint the window here.
    fn redraw(&mut self, ctx: &MarsAppCtx);
}

/// Handle passed to every `MarsApp` callback.  Owns the NSWindow /
/// NSView retain counts; lets the app request redraws or exit.
pub struct MarsAppCtx {
    inner: Retained<MarsView>,
    nswindow: Retained<NSWindow>,
    nsapp: Retained<NSApplication>,
    redraw_pending: Cell<bool>,
    exit_requested: Cell<bool>,
}

impl MarsAppCtx {
    /// The NSView the renderer attaches to.
    pub fn ns_view(&self) -> &NSView {
        // MarsView ⊆ NSView (subclass); deref via cast.
        unsafe { &*(Retained::as_ptr(&self.inner) as *const NSView) }
    }

    /// Window content size in physical (backing) pixels.
    pub fn inner_size_phys(&self) -> (f64, f64) {
        let bounds = self.ns_view().bounds();
        let scale = self.scale();
        (bounds.size.width * scale, bounds.size.height * scale)
    }

    /// Backing scale factor of the window's current screen.
    pub fn scale(&self) -> f64 {
        self.nswindow.backingScaleFactor() as f64
    }

    /// Schedule a `MarsApp::redraw` after the current event handler
    /// returns.  Coalesced — multiple calls per event collapse.
    pub fn request_redraw(&self) {
        self.redraw_pending.set(true);
    }

    /// Schedule the run loop to stop after the current handler
    /// returns.  After exit, `run_app` returns.
    pub fn exit(&self) {
        self.exit_requested.set(true);
    }
}

/// Window-creation parameters.  Logical (point) coordinates.
#[derive(Clone, Debug)]
pub struct WindowAttrs {
    pub title: String,
    pub width_logical: f64,
    pub height_logical: f64,
}

/// Cross-thread wake signal.  Construct once before run_app starts;
/// clones are cheap (`Arc` bump) and `Send + Sync`.  `wake()` from
/// any thread schedules a `MarsApp::user_event` on the main thread.
pub struct EventProxy {
    inner: Arc<ProxyInner>,
}

struct ProxyInner {
    source: SendableSource,
    runloop: OnceLock<SendableRunLoop>,
}

struct SendableSource(CFRunLoopSourceRef);
struct SendableRunLoop(CFRunLoopRef);

// SAFETY: CFRunLoopSourceRef and CFRunLoopRef are CFType handles;
// CFRunLoopSourceSignal / CFRunLoopWakeUp are documented as
// thread-safe on these handles.
unsafe impl Send for SendableSource {}
unsafe impl Sync for SendableSource {}
unsafe impl Send for SendableRunLoop {}
unsafe impl Sync for SendableRunLoop {}

impl EventProxy {
    pub fn new() -> Self {
        // The source's perform callback is a no-op — it just bumps
        // the main run loop.  Actual work happens in our drain pass:
        // anywhere we re-enter user code we also drain the proxy.
        // (Modeled on winit's macOS proxy: signaling unblocks the
        // run loop, dispatch happens via a separate observer.)
        let mut ctx = CFRunLoopSourceContext {
            version: 0,
            info: ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: source_perform,
        };
        let source =
            unsafe { CFRunLoopSourceCreate(ptr::null_mut(), 0 as CFIndex, &mut ctx) };
        assert!(!source.is_null(), "CFRunLoopSourceCreate failed");
        Self {
            inner: Arc::new(ProxyInner {
                source: SendableSource(source),
                runloop: OnceLock::new(),
            }),
        }
    }

    pub fn wake(&self) {
        unsafe {
            CFRunLoopSourceSignal(self.inner.source.0);
            if let Some(rl) = self.inner.runloop.get() {
                CFRunLoopWakeUp(rl.0);
            }
        }
    }
}

impl Default for EventProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for EventProxy {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

extern "C" fn source_perform(_info: *const c_void) {
    // Wake fires user_event.  Any redraws or exits the handler
    // requests are flushed by `dispatch_event`.
    dispatch_event(EventKind::UserEvent);
}

// ---------------------------------------------------------------------------
// Custom NSView subclass
// ---------------------------------------------------------------------------

/// Per-instance state for the custom view.  Empty for now; ivars
/// are set by `set_ivars` to satisfy `DeclaredClass` even when we
/// have no fields.
pub struct MarsViewIvars;

declare_class!(
    /// `NSView` subclass that captures key + mouse + scroll events.
    /// We override `acceptsFirstResponder` so this view, not the
    /// window itself, is the first responder for keyboard input.
    pub struct MarsView;

    // SAFETY:
    // - Superclass NSView has no special subclassing requirements.
    // - Main-thread mutability is correct for an NSView subclass.
    // - MarsView holds no Drop-relevant state.
    unsafe impl ClassType for MarsView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "MarsView";
    }

    impl DeclaredClass for MarsView {
        type Ivars = MarsViewIvars;
    }

    unsafe impl NSObjectProtocol for MarsView {}

    unsafe impl MarsView {
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // Flip Y-axis so origin is top-left (matches the rest of the
        // code base's convention; layout / hit-testing assume top-left).
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            if let Some(ev) = nsevent_to_mars_key(event, KeyState::Pressed) {
                let mods = nsevent_modifiers(event);
                dispatch_event(EventKind::Key(ev, mods));
            }
        }

        #[method(keyUp:)]
        fn key_up(&self, event: &NSEvent) {
            if let Some(ev) = nsevent_to_mars_key(event, KeyState::Released) {
                let mods = nsevent_modifiers(event);
                dispatch_event(EventKind::Key(ev, mods));
            }
        }

        #[method(flagsChanged:)]
        fn flags_changed(&self, event: &NSEvent) {
            // Modifiers carry on the event object; we surface them via
            // the next key_event delivery.  For mouse-only sessions
            // this still works because Mars currently doesn't gate any
            // mouse behaviour on modifiers.  No callback to MarsApp
            // here — modifiers ride along with the keystrokes that
            // actually arrive.
            let _ = event;
        }

        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            // locationInWindow is in window coords (logical points,
            // origin bottom-left of window).  Convert into our flipped
            // view's coords (origin top-left, logical points), then to
            // backing pixels.
            let loc_window = unsafe { event.locationInWindow() };
            let loc_view =
                self.convertPoint_fromView(loc_window, None);
            let backing = unsafe { self.convertPointToBacking(loc_view) };
            // convertPointToBacking flips Y back to bottom-left in the
            // returned point's frame; with isFlipped=true on the view,
            // the y is already top-down in `loc_view`, and
            // convertPointToBacking just scales — but Apple actually
            // negates y here regardless of `isFlipped`.  Compute via
            // the bounds height instead so we get a top-left origin
            // in backing pixels deterministically.
            let view_h_phys = self.bounds().size.height
                * self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let x_phys = backing.x.abs();
            let y_phys = if backing.y < 0.0 {
                view_h_phys + backing.y
            } else {
                backing.y
            };
            dispatch_event(EventKind::MouseDown { x: x_phys, y: y_phys });
        }

        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            // hasPreciseScrollingDeltas distinguishes trackpads
            // (pixel-precise) from wheels (line-stepped).  Scale on
            // the caller side via cell height.
            let precise = unsafe { event.hasPreciseScrollingDeltas() };
            let dx;
            let dy;
            if precise {
                // In points; convert to backing pixels.
                let scale =
                    self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
                dx = unsafe { event.scrollingDeltaX() } * scale;
                dy = unsafe { event.scrollingDeltaY() } * scale;
            } else {
                // Lines; pass through, caller scales.
                dx = unsafe { event.deltaX() };
                dy = unsafe { event.deltaY() };
            }
            dispatch_event(EventKind::Scroll { dx, dy, precise });
        }
    }
);

// ---------------------------------------------------------------------------
// NSWindowDelegate subclass
// ---------------------------------------------------------------------------

pub struct MarsWindowDelegateIvars;

declare_class!(
    pub struct MarsWindowDelegate;

    // SAFETY:
    // - Superclass NSObject has no subclassing requirements.
    // - Window delegates are main-thread-only.
    // - No Drop logic here.
    unsafe impl ClassType for MarsWindowDelegate {
        type Super = objc2::runtime::NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "MarsWindowDelegate";
    }

    impl DeclaredClass for MarsWindowDelegate {
        type Ivars = MarsWindowDelegateIvars;
    }

    unsafe impl NSObjectProtocol for MarsWindowDelegate {}

    unsafe impl NSWindowDelegate for MarsWindowDelegate {
        #[method(windowShouldClose:)]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            dispatch_event(EventKind::CloseRequested);
            // Returning false lets the app handle the close; if the
            // app calls ctx.exit() inside close_requested, the run
            // loop stops and we return.  Mars's current behaviour is
            // "close = exit", but we honour the indirection cleanly.
            false
        }

        #[method(windowDidResize:)]
        fn window_did_resize(&self, _notification: &NSNotification) {
            dispatch_event(EventKind::Resized);
        }

        #[method(windowDidBecomeKey:)]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            dispatch_event(EventKind::Focused(true));
        }

        #[method(windowDidResignKey:)]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            dispatch_event(EventKind::Focused(false));
        }
    }
);

// ---------------------------------------------------------------------------
// Event dispatch
// ---------------------------------------------------------------------------

enum EventKind {
    UserEvent,
    Key(MarsKeyEvent, Modifiers),
    MouseDown { x: f64, y: f64 },
    Scroll { dx: f64, dy: f64, precise: bool },
    Resized,
    Focused(bool),
    CloseRequested,
}

struct AppState {
    app: Box<dyn MarsApp>,
    ctx: MarsAppCtx,
}

thread_local! {
    /// Owns the user's `MarsApp` and the per-window context.
    /// Populated by `run_app`; accessed from every event handler.
    static APP_STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

fn dispatch_event(kind: EventKind) {
    APP_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else { return };
        let AppState { app, ctx } = state;

        match kind {
            EventKind::UserEvent => app.user_event(ctx),
            EventKind::Key(ev, mods) => app.key_event(ctx, ev, mods),
            EventKind::MouseDown { x, y } => app.mouse_down(ctx, x, y),
            EventKind::Scroll { dx, dy, precise } => app.scroll(ctx, dx, dy, precise),
            EventKind::Resized => {
                let (w, h) = ctx.inner_size_phys();
                app.resized(ctx, w, h);
            }
            EventKind::Focused(f) => app.focused(ctx, f),
            EventKind::CloseRequested => app.close_requested(ctx),
        }

        if ctx.redraw_pending.replace(false) {
            app.redraw(ctx);
        }
        if ctx.exit_requested.get() {
            ctx.nsapp.stop(None);
            // NSApp.stop only takes effect when the next event arrives.
            // Post a synthetic no-op event so the loop unblocks promptly.
            post_dummy_event(&ctx.nsapp);
        }
    });
}

fn post_dummy_event(nsapp: &NSApplication) {
    // Construct a no-op application-defined event so NSApp.run sees
    // an event after stop() and returns.  Type 15 is NSEventTypeApplicationDefined.
    use objc2::class;
    use objc2::msg_send;
    unsafe {
        let event_cls = class!(NSEvent);
        let nsevent: *mut NSEvent = msg_send![event_cls,
            otherEventWithType: 15u64 /* NSEventTypeApplicationDefined */
            location: NSPoint::new(0.0, 0.0)
            modifierFlags: 0u64
            timestamp: 0.0_f64
            windowNumber: 0_isize
            context: ptr::null::<c_void>()
            subtype: 0_i16
            data1: 0_isize
            data2: 0_isize
        ];
        if !nsevent.is_null() {
            let nsevent = &*nsevent;
            nsapp.postEvent_atStart(nsevent, true);
        }
    }
}

// ---------------------------------------------------------------------------
// run_app
// ---------------------------------------------------------------------------

/// Block on the AppKit run loop, dispatching events into `app`.
/// Returns when `ctx.exit()` has been called and the run loop drains.
pub fn run_app<A: MarsApp>(mut app: A, proxy: EventProxy, attrs: WindowAttrs) {
    let mtm = MainThreadMarker::new()
        .expect("run_app must be called on the main thread");
    let nsapp = NSApplication::sharedApplication(mtm);
    nsapp.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // 1. Create custom NSView (origin top-left) sized to logical attrs.
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(attrs.width_logical, attrs.height_logical),
    );
    let view: Retained<MarsView> = {
        let alloc = mtm.alloc::<MarsView>().set_ivars(MarsViewIvars);
        unsafe { msg_send_id![super(alloc), initWithFrame: frame] }
    };

    // 2. Create NSWindow with view as content.
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window: Retained<NSWindow> = unsafe {
        let alloc = mtm.alloc::<NSWindow>();
        msg_send_id![alloc,
            initWithContentRect: frame,
            styleMask: style,
            backing: NSBackingStoreType::NSBackingStoreBuffered,
            defer: false
        ]
    };
    window.setTitle(&NSString::from_str(&attrs.title));
    window.setContentView(Some(unsafe {
        &*(Retained::as_ptr(&view) as *const NSView)
    }));
    window.setAcceptsMouseMovedEvents(false);
    unsafe { window.makeFirstResponder(Some(&view)) };

    // 3. Window delegate.
    let delegate: Retained<MarsWindowDelegate> = {
        let alloc = mtm
            .alloc::<MarsWindowDelegate>()
            .set_ivars(MarsWindowDelegateIvars);
        unsafe { msg_send_id![super(alloc), init] }
    };
    let proto: &ProtocolObject<dyn NSWindowDelegate> =
        ProtocolObject::from_ref(&*delegate);
    window.setDelegate(Some(proto));

    // 4. Register the proxy's source on the main run loop.  Wakes
    //    posted before this point are sticky on the source and fire
    //    on the first iteration.
    let main_rl = unsafe { CFRunLoopGetMain() };
    unsafe {
        CFRunLoopAddSource(main_rl, proxy.inner.source.0, kCFRunLoopCommonModes);
    }
    let _ = proxy.inner.runloop.set(SendableRunLoop(main_rl));

    // 5. Build the ctx and stash app state.  After this the NSView
    //    callbacks are live.
    let ctx = MarsAppCtx {
        inner: view.clone(),
        nswindow: window.clone(),
        nsapp: nsapp.clone(),
        redraw_pending: Cell::new(false),
        exit_requested: Cell::new(false),
    };

    // Show + focus + activate.
    nsapp.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);

    // Hand control to the app's resumed handler.  Borrow the cell as
    // `Box<dyn MarsApp>` so `dispatch_event` and resumed share state.
    APP_STATE.with(|cell| {
        *cell.borrow_mut() = Some(AppState {
            app: Box::new(app),
            ctx,
        });
    });

    APP_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.as_mut().unwrap();
        state.app.resumed(&state.ctx);
        // Deliver an initial Resized so the app sizes its renderer.
        let (w, h) = state.ctx.inner_size_phys();
        state.app.resized(&state.ctx, w, h);
        if state.ctx.redraw_pending.replace(false) {
            state.app.redraw(&state.ctx);
        }
    });

    // 6. Run.  Returns after dispatch_event sees an exit request.
    unsafe { nsapp.run() };

    // Drop app state on the main thread so `Drop`s for sessions /
    // renderers fire here.
    APP_STATE.with(|cell| *cell.borrow_mut() = None);
}

// ---------------------------------------------------------------------------
// NSEvent → MarsKeyEvent
// ---------------------------------------------------------------------------

fn nsevent_modifiers(event: &NSEvent) -> Modifiers {
    let flags = unsafe { event.modifierFlags() };
    Modifiers {
        shift: flags.contains(NSEventModifierFlags::NSEventModifierFlagShift),
        control: flags.contains(NSEventModifierFlags::NSEventModifierFlagControl),
        alt: flags.contains(NSEventModifierFlags::NSEventModifierFlagOption),
        super_: flags.contains(NSEventModifierFlags::NSEventModifierFlagCommand),
    }
}

fn nsevent_to_mars_key(event: &NSEvent, state: KeyState) -> Option<MarsKeyEvent> {
    let key_code = unsafe { event.keyCode() };
    let logical = match key_code {
        // Carbon HIToolbox keyCodes — stable across macOS versions.
        0x24 => LogicalKey::Named(NamedKey::Enter),
        0x4C => LogicalKey::Named(NamedKey::Enter), // numeric-keypad Enter
        0x30 => LogicalKey::Named(NamedKey::Tab),
        0x33 => LogicalKey::Named(NamedKey::Backspace),
        0x35 => LogicalKey::Named(NamedKey::Escape),
        0x7E => LogicalKey::Named(NamedKey::ArrowUp),
        0x7D => LogicalKey::Named(NamedKey::ArrowDown),
        0x7B => LogicalKey::Named(NamedKey::ArrowLeft),
        0x7C => LogicalKey::Named(NamedKey::ArrowRight),
        _ => {
            // Modifier-independent character.  charactersIgnoringModifiers
            // gives "a" for both `a` and `shift+a`.
            let s_opt = unsafe { event.charactersIgnoringModifiers() };
            match s_opt.and_then(|s| s.to_string().chars().next()) {
                Some(c) => LogicalKey::Char(c),
                None => LogicalKey::Other,
            }
        }
    };

    // Resolved text — what the keystroke would type with current
    // modifiers applied.  None for navigation keys / pure modifier
    // presses.
    let text = if matches!(logical, LogicalKey::Named(_)) {
        None
    } else {
        unsafe { event.characters() }.map(|s| s.to_string())
    };

    Some(MarsKeyEvent {
        state,
        logical,
        text,
    })
}

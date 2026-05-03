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
//! ## Known differences vs winit
//!
//! - **IME via NSTextInputClient.**  `MarsView` implements the
//!   protocol so CJK / Japanese / emoji input works.  Side-effect:
//!   live-PTY throughput drops ~5-10% even when no IME is composing,
//!   because AppKit treats text-input-clients differently in event
//!   dispatch.  Trade accepted in exchange for the feature.
//! - **No `CursorMoved` event.**  Mars only inspects the cursor at
//!   click time; we read `locationInWindow` from `mouseDown:` instead.
//! - **No inline preedit rendering.**  macOS draws its own candidate
//!   window above the caret, but the marked text isn't currently
//!   composited into the terminal grid.  Acceptable v1.

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
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent,
    NSEventModifierFlags, NSTextInputClient, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSAttributedStringKey, NSNotFound,
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString,
    NSUInteger,
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

/// Per-instance state for the custom view.
pub struct MarsViewIvars {
    /// Marked (pre-edit) string from the active IME composition.
    /// Stored to honour the NSTextInputClient `markedRange` /
    /// `hasMarkedText` queries; not yet rendered as inline preview.
    /// (macOS draws its own candidate window above the caret, so
    /// the user can still see what they're composing.)
    marked_text: RefCell<String>,
    /// True after one of `insertText` / `setMarkedText` /
    /// `doCommandBySelector` (with a mapped selector) has handled
    /// the event during a `keyDown` dispatch.  Reset at the top of
    /// every `keyDown`; checked after `interpretKeyEvents` returns
    /// to decide whether to fall through to the raw-key path.
    ime_consumed: Cell<bool>,
    /// Modifier state captured when `keyDown` fires.  IME callbacks
    /// (`insertText`, `doCommandBySelector`) don't carry an
    /// `NSEvent`; we use this to forward modifiers to the app.
    last_modifiers: Cell<Modifiers>,
}

declare_class!(
    /// `NSView` subclass that captures key + mouse + scroll events
    /// and bridges them into `MarsApp`.  Implements
    /// `NSTextInputClient` so CJK / emoji IMEs can compose into the
    /// terminal.
    pub struct MarsView;

    // SAFETY:
    // - Superclass NSView has no special subclassing requirements.
    // - `#[inherits(NSResponder, NSObject)]` exposes NSResponder
    //   methods (notably `interpretKeyEvents`).
    // - Main-thread mutability is correct for an NSView subclass.
    // - Drop-relevant state in ivars is safe inside RefCell/Cell.
    unsafe impl ClassType for MarsView {
        #[inherits(objc2_app_kit::NSResponder, objc2::runtime::NSObject)]
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
            let mods = nsevent_modifiers(event);

            // Cmd / Ctrl combos bypass the IME entirely.  This keeps
            // shortcuts that the rest of mars expects (Cmd-V paste,
            // Ctrl-C → 0x03, Ctrl-[ → ESC) working — IMEs typically
            // don't consume these but we don't want to depend on that.
            if mods.super_ || mods.control {
                if let Some(ev) = nsevent_to_mars_key(event, KeyState::Pressed) {
                    dispatch_event(EventKind::Key(ev, mods));
                }
                return;
            }

            self.ivars().ime_consumed.set(false);
            self.ivars().last_modifiers.set(mods);

            // interpretKeyEvents synchronously calls back into our
            // NSTextInputClient methods (insertText / setMarkedText /
            // doCommandBySelector) for the matching events.  When IME
            // is composing CJK / emoji, only setMarkedText fires.
            // When it commits, insertText fires.  When the key was
            // navigation / editing, doCommandBySelector fires.
            let array = NSArray::from_slice(&[event]);
            unsafe { self.interpretKeyEvents(&array) };

            // Nothing IME-relevant fired (e.g. an unmapped function
            // key).  Fall back to raw nsevent translation so the app
            // still sees a key_event.
            if !self.ivars().ime_consumed.get() {
                if let Some(ev) = nsevent_to_mars_key(event, KeyState::Pressed) {
                    dispatch_event(EventKind::Key(ev, mods));
                }
            }
        }

        #[method(keyUp:)]
        fn key_up(&self, event: &NSEvent) {
            // Released events bypass IME — IMEs only consume key-down.
            if let Some(ev) = nsevent_to_mars_key(event, KeyState::Released) {
                let mods = nsevent_modifiers(event);
                dispatch_event(EventKind::Key(ev, mods));
            }
        }

        #[method(flagsChanged:)]
        fn flags_changed(&self, event: &NSEvent) {
            // Modifiers carry on the event object; we surface them via
            // the next key_event delivery.  No callback to MarsApp
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

    unsafe impl NSTextInputClient for MarsView {
        // Required: queries

        #[method(hasMarkedText)]
        fn has_marked_text(&self) -> bool {
            !self.ivars().marked_text.borrow().is_empty()
        }

        #[method(markedRange)]
        fn marked_range(&self) -> NSRange {
            let len = self.ivars().marked_text.borrow().len();
            if len > 0 {
                NSRange::new(0, len as NSUInteger)
            } else {
                NSRange::new(NSNotFound as NSUInteger, 0)
            }
        }

        #[method(selectedRange)]
        fn selected_range(&self) -> NSRange {
            // We don't maintain a selection model.  NSNotFound is
            // documented to mean "no selection".
            NSRange::new(NSNotFound as NSUInteger, 0)
        }

        #[method_id(validAttributesForMarkedText)]
        fn valid_attributes_for_marked_text(
            &self,
        ) -> Retained<NSArray<NSAttributedStringKey>> {
            // Empty → IME defaults to plain text only.  We don't
            // honour underline / colour styling on preedit anyway.
            NSArray::new()
        }

        #[method_id(attributedSubstringForProposedRange:actualRange:)]
        fn attributed_substring_for_proposed_range(
            &self,
            _range: NSRange,
            _actual_range: *mut NSRange,
        ) -> Option<Retained<NSAttributedString>> {
            // We don't expose buffered terminal text to the IME.
            None
        }

        #[method(characterIndexForPoint:)]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            0
        }

        #[method(firstRectForCharacterRange:actualRange:)]
        fn first_rect_for_character_range(
            &self,
            _range: NSRange,
            _actual_range: *mut NSRange,
        ) -> NSRect {
            // Returning a zero rect lands the IME candidate window
            // in macOS's default position.  Plumbing through the
            // real cursor position needs Mars-side state that we
            // don't surface yet — follow-up.
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        }

        // Required: composition lifecycle

        #[method(setMarkedText:selectedRange:replacementRange:)]
        fn set_marked_text(
            &self,
            string: &NSObject,
            _selected_range: NSRange,
            _replacement_range: NSRange,
        ) {
            self.ivars().ime_consumed.set(true);
            let s = nsobject_string_to_string(string);
            *self.ivars().marked_text.borrow_mut() = s;
            // Mars doesn't render preedit inline yet — the IME's own
            // candidate window covers UX.  When we add inline preview
            // (Phase E?), this is where we'd notify the app.
        }

        #[method(unmarkText)]
        fn unmark_text(&self) {
            self.ivars().marked_text.borrow_mut().clear();
        }

        #[method(insertText:replacementRange:)]
        fn insert_text(&self, string: &NSObject, _replacement_range: NSRange) {
            self.ivars().ime_consumed.set(true);
            let s = nsobject_string_to_string(string);
            self.ivars().marked_text.borrow_mut().clear();
            if s.is_empty() {
                return;
            }
            // Treat IME-committed text as a single key-event with
            // logical=Other and the resolved text payload.  The
            // text fallback in input::key_event_to_bytes ships it
            // straight to the PTY, regardless of byte width.
            let ev = MarsKeyEvent {
                state: KeyState::Pressed,
                logical: LogicalKey::Other,
                text: Some(s),
            };
            let mods = self.ivars().last_modifiers.get();
            dispatch_event(EventKind::Key(ev, mods));
        }

        #[method(doCommandBySelector:)]
        fn do_command_by_selector(&self, selector: Sel) {
            // interpretKeyEvents calls this for keys the IME didn't
            // consume that map to standard editing commands.  We
            // translate the well-known selectors to NamedKey so the
            // PTY sees Enter / Tab / Esc / Backspace / arrows.
            // Unknown selectors leave ime_consumed=false so the
            // raw-key fallback in keyDown still fires.
            let named = match selector_named_key(selector) {
                Some(k) => k,
                None => return,
            };
            self.ivars().ime_consumed.set(true);
            let ev = MarsKeyEvent {
                state: KeyState::Pressed,
                logical: LogicalKey::Named(named),
                text: None,
            };
            let mods = self.ivars().last_modifiers.get();
            dispatch_event(EventKind::Key(ev, mods));
        }
    }
);

/// Map a Cocoa `NSStandardKeyBindingResponding` selector — fired by
/// `interpretKeyEvents` for non-text key presses — to our NamedKey
/// enum.  Returns None for selectors we don't translate (typically
/// `noop:`, fired for ctrl-letter combos that we already routed
/// through the raw path before entering IME).
fn selector_named_key(selector: Sel) -> Option<NamedKey> {
    match selector.name() {
        "insertNewline:" | "insertLineBreak:" | "insertNewlineIgnoringFieldEditor:" => {
            Some(NamedKey::Enter)
        }
        "insertTab:" | "insertTabIgnoringFieldEditor:" => Some(NamedKey::Tab),
        "deleteBackward:" => Some(NamedKey::Backspace),
        "cancelOperation:" | "complete:" => Some(NamedKey::Escape),
        "moveUp:" | "moveUpAndModifySelection:" | "moveToBeginningOfDocument:" => {
            Some(NamedKey::ArrowUp)
        }
        "moveDown:" | "moveDownAndModifySelection:" | "moveToEndOfDocument:" => {
            Some(NamedKey::ArrowDown)
        }
        "moveLeft:" | "moveLeftAndModifySelection:" | "moveBackward:" => {
            Some(NamedKey::ArrowLeft)
        }
        "moveRight:" | "moveRightAndModifySelection:" | "moveForward:" => {
            Some(NamedKey::ArrowRight)
        }
        _ => None,
    }
}

/// Extract a Rust String from an NSString or NSAttributedString
/// reference (the protocol declares `&AnyObject` for these
/// arguments — runtime says it's always one of those two classes).
fn nsobject_string_to_string(string: &NSObject) -> String {
    if string.is_kind_of::<NSAttributedString>() {
        let p: *const NSObject = string;
        let p: *const NSAttributedString = p.cast();
        unsafe { (*p).string().to_string() }
    } else {
        let p: *const NSObject = string;
        let p: *const NSString = p.cast();
        unsafe { (*p).to_string() }
    }
}

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
        let alloc = mtm.alloc::<MarsView>().set_ivars(MarsViewIvars {
            marked_text: RefCell::new(String::new()),
            ime_consumed: Cell::new(false),
            last_modifiers: Cell::new(Modifiers::default()),
        });
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

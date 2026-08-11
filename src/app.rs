//! Marspot's AppKit-direct window + event loop.
//!
//! Replaces winit.  We talk to NSApplication / NSWindow / NSView
//! through `objc2-app-kit` and dispatch events into a `MarspotApp` impl.
//!
//! The shape mirrors winit's `ApplicationHandler` (resumed → events
//! → user_event) but on a smaller surface — only the events Marspot
//! actually consumes.  Lives in the lib because both `marspot` and
//! `mcli` use it.
//!
//! ## Threading
//!
//! Everything runs on the main thread except `EventProxy::wake()`,
//! which can be called from any thread.  Wake signals a
//! CFRunLoopSource registered on the main run loop; on the next
//! main-thread iteration the source's perform fires `MarspotApp::user_event`.
//!
//! ## Known differences vs winit
//!
//! - **IME via NSTextInputClient.**  `MarspotView` implements the
//!   protocol so CJK / Japanese / emoji input works.  An initial
//!   5-trial A/B suggested a 5–10 % live-PTY throughput drop, but a
//!   follow-up 10-trial Welch t-test (cat-cjk) gave t=1.08 — well
//!   below the 95 % significance threshold — so the apparent
//!   regression is within run-to-run noise.  Treat it as "no
//!   measured regression at current precision."
//! - **No `CursorMoved` event.**  Marspot only inspects the cursor at
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
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSColor, NSDragOperation, NSDraggingInfo,
    NSEvent, NSEventModifierFlags, NSImage, NSPasteboardTypeFileURL, NSTextInputClient,
    NSTitlebarSeparatorStyle, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSCopying,
    NSNotFound, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize,
    NSString, NSUInteger, NSURL,
};

use crate::input::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

/// Trait that the binary's main loop implements.  Methods are called
/// on the main thread; the `MarspotAppCtx` argument lets handlers
/// request a redraw or schedule exit.
pub trait MarspotApp: 'static {
    /// Called once after the window is created and the run loop has
    /// started.  Renderer setup belongs here.
    fn resumed(&mut self, ctx: &MarspotAppCtx);

    /// Called whenever `EventProxy::wake()` is signalled from any
    /// thread.  Coalesced — multiple wakes between iterations may
    /// collapse into one call.
    fn user_event(&mut self, ctx: &MarspotAppCtx);

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, modifiers: Modifiers);

    /// Mouse-down with location in physical pixels, origin top-left.
    fn mouse_down(&mut self, ctx: &MarspotAppCtx, x_phys: f64, y_phys: f64, modifiers: Modifiers);

    /// Right-mouse-down (AppKit `rightMouseDown:`).  Same coord
    /// convention as `mouse_down`.  Default no-op so existing apps
    /// (mcli / shell / snapshot) don't have to care — Marspot
    /// itself overrides to drive the context menu.
    ///
    /// Note: macOS converts Ctrl-Left-Click into rightMouseDown by
    /// default at the AppKit layer (`controlMouseClicksOpenMenus`
    /// is the system default), so this handler also receives that
    /// path — caller doesn't need to special-case it.
    fn mouse_right_down(
        &mut self,
        _ctx: &MarspotAppCtx,
        _x_phys: f64,
        _y_phys: f64,
        _modifiers: Modifiers,
    ) {
    }

    /// Mouse-dragged (button still pressed) at physical-pixel `(x, y)`.
    /// Default implementation is a no-op — apps that want drag/select
    /// behaviour override this.
    fn mouse_drag(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64, _hover_window_id: u32, _hover_x: f64, _hover_y: f64) {}

    /// Mouse-moved (no button) at physical-pixel `(x, y)`.  Delivered
    /// only when the window is key and `setAcceptsMouseMovedEvents`
    /// is true.  Default no-op — apps that want hover affordances
    /// override this.
    fn mouse_moved(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64) {}

    /// Mouse-up at physical-pixel `(x, y)`.  Default no-op.
    fn mouse_up(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64, _drop_window_id: u32, _drop_x: f64, _drop_y: f64) {}

    /// Finder file drop at physical-pixel `(x, y)` with ≥1 resolved
    /// filesystem paths.  Default no-op — the terminal apps override
    /// to insert shell-quoted paths into the pane under the cursor.
    fn file_drop(&mut self, _ctx: &MarspotAppCtx, _x_phys: f64, _y_phys: f64, _paths: &[String]) {
    }

    /// Scroll delta in physical pixels (positive Y = scroll content
    /// down).  `precise` is true for trackpad / Magic Mouse, false
    /// for traditional mouse wheels (where deltas come in lines).
    fn scroll(&mut self, ctx: &MarspotAppCtx, dx_phys: f64, dy_phys: f64, precise: bool);

    /// Window content size in physical pixels.  Fired on every step
    /// of a live resize plus once after the window is initially shown.
    fn resized(&mut self, ctx: &MarspotAppCtx, width_phys: f64, height_phys: f64);

    /// F3+6.1 — fired on `windowDidMove:` so apps can persist window
    /// frame on drag.  Default no-op so existing apps don't have to
    /// care.  Use `ctx.window_frame_pt()` to read the new frame.
    fn moved(&mut self, _ctx: &MarspotAppCtx) {}

    fn focused(&mut self, ctx: &MarspotAppCtx, focused: bool);

    /// A window this app asked for has been created.  `ctx` is that
    /// window's own context — which is what makes it the right place
    /// to size its IOSurface pair.  Default: nothing to do.
    fn window_opened(&mut self, _ctx: &MarspotAppCtx) {}

    fn close_requested(&mut self, ctx: &MarspotAppCtx);

    /// IME preedit ("marked text") changed.  Empty string means the
    /// composition was committed or cancelled.  Apps that render an
    /// inline preview override this; default is a no-op so mcli /
    /// snapshot paths don't have to care.
    fn ime_preedit_changed(&mut self, _ctx: &MarspotAppCtx, _text: &str) {}

    /// Fired when a previous `ctx.request_redraw()` is being honoured.
    /// Repaint the window here.
    fn redraw(&mut self, ctx: &MarspotAppCtx);

    /// The dev panel's independent `NSWindow` was moved / resized /
    /// changed displays.  Apps that persist dev-window geometry hook
    /// this to save state.  Default no-op so non-dev-panel apps don't
    /// have to care.  The `dev_window` module is the data source —
    /// query `dev_window::with_dev_window(|w| w.frame_pt())` etc.
    fn dev_window_changed(&mut self, _ctx: &MarspotAppCtx) {}

    /// User clicked inside the dev panel NSWindow's content area.
    /// Coords are logical pt (view-local, `isFlipped` → y=0 top).
    /// Default no-op; L1 overrides to hit-test against tab strip /
    /// menu and mutate `DevPanelState`.
    fn dev_panel_click(&mut self, _ctx: &MarspotAppCtx, _x_pt: f64, _y_pt: f64) {}
    fn dev_panel_scroll(&mut self, _ctx: &MarspotAppCtx, _delta_y_pt: f64) {}
}

/// Handle passed to every `MarspotApp` callback.  Owns the NSWindow /
/// NSView retain counts; lets the app request redraws or exit.
pub struct MarspotAppCtx {
    /// RFC-005 — stable id of the window this context drives.  L1
    /// allocates it; it is what every input frame carries and what
    /// the core resolves a `WindowState` from.
    window_id: u32,
    inner: Retained<MarspotView>,
    nswindow: Retained<NSWindow>,
    nsapp: Retained<NSApplication>,
    /// Kept alive here on purpose: `NSWindow.delegate` is a **weak**
    /// reference.  The boot window got away with a local because
    /// `run_app` never returns, but a window opened later would lose
    /// every resize / close / focus callback the moment its builder
    /// returned.
    _delegate: Retained<MarspotWindowDelegate>,
    redraw_pending: Cell<bool>,
    exit_requested: Cell<bool>,
}

impl MarspotAppCtx {
    /// Which window this context drives.
    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    /// The NSView the renderer attaches to.
    pub fn ns_view(&self) -> &NSView {
        // MarspotView ⊆ NSView (subclass); deref via cast.
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

    /// Right edge of the macOS traffic-light cluster, in **physical
    /// px** from the window's left edge — `0.0` when the cluster is
    /// not on screen.
    ///
    /// Asked of AppKit rather than assumed.  The toolbar used to start
    /// at a constant 84 pt, a number obtained by screen-capturing the
    /// live window and measuring the discs; in full screen AppKit
    /// takes the cluster away and that reservation became a hole the
    /// toolbar sat to the right of (2026-08-11 report).  The buttons
    /// know where they are and whether they are hidden, so the layout
    /// asks them.
    pub fn traffic_lights_right_phys(&self) -> f64 {
        use objc2_app_kit::NSWindowButton;
        let scale = self.scale();
        let view_h = self.ns_view().bounds().size.height;
        [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ]
        .into_iter()
        .filter_map(|b| self.nswindow.standardWindowButton(b))
        .filter(|btn| {
            // Full screen leaves the buttons *present* but hidden, and
            // parents them into an overlay that slides in on hover —
            // a hidden button's frame is still its old one, so the
            // hidden flag is the part that matters.
            let hidden = btn.isHidden();
            let on_window = btn
                .window()
                .map(|w| std::ptr::eq(&*w, &*self.nswindow))
                .unwrap_or(false);
            !hidden && on_window
        })
        .filter_map(|btn| {
            let f = btn.frame();
            let sup = unsafe { btn.superview() }?;
            // Button frame is in its superview's coords; convert to
            // the window's, which is what the layout measures from.
            let p = sup.convertPoint_toView(
                NSPoint::new(f.origin.x + f.size.width, f.origin.y),
                None,
            );
            let _ = view_h;
            Some(p.x * scale)
        })
        .fold(0.0f64, f64::max)
    }

    /// Schedule a `MarspotApp::redraw` after the current event handler
    /// returns.  Coalesced — multiple calls per event collapse.
    pub fn request_redraw(&self) {
        self.redraw_pending.set(true);
    }

    /// Publish the focused-pane caret rect to the view so the IME
    /// candidate window anchors under the caret.  Coordinates are
    /// **view-local physical pixels** (top-left origin, y-down — same
    /// space the renderer paints in).  Pass `None` to clear (e.g. no
    /// visible caret).
    pub fn set_caret_rect_phys(&self, rect: Option<(f64, f64, f64, f64)>) {
        let nsr = rect.map(|(x, y, w, h)| {
            NSRect::new(NSPoint::new(x, y), NSSize::new(w.max(1.0), h.max(1.0)))
        });
        self.inner.ivars().caret_view_phys_rect.set(nsr);
    }

    /// Schedule the run loop to stop after the current handler
    /// returns.  After exit, `run_app` returns.
    pub fn exit(&self) {
        self.exit_requested.set(true);
    }

    /// Current window frame in **screen points, AppKit native**
    /// (bottom-left origin).  Round-trips losslessly through
    /// `WindowAttrs::frame_pt` so a successor process (shell
    /// self-update execv) can reopen at the exact same place.
    pub fn window_frame_pt(&self) -> (f64, f64, f64, f64) {
        let f = self.nswindow.frame();
        (f.origin.x, f.origin.y, f.size.width, f.size.height)
    }

    /// F3+6.1 — `CGDirectDisplayID` (u32) of the screen the window
    /// is currently on, via `[NSWindow screen].deviceDescription[
    /// @"NSScreenNumber"]`.  None when the window isn't attached to a
    /// screen (off-screen / between displays during a drag).  Used by
    /// the persistence layer to re-anchor on the same monitor across
    /// L1 restarts.
    pub fn window_display_id(&self) -> Option<u32> {
        unsafe {
            let screen = self.nswindow.screen()?;
            let desc = screen.deviceDescription();
            // NSScreenNumber is an NSNumber wrapped in the NSScreen's
            // device-description dictionary; pull it out via objc2's
            // NSDictionary indexing + downcast to NSNumber.
            use objc2_foundation::{NSNumber, NSString};
            let key: Retained<NSString> = NSString::from_str("NSScreenNumber");
            let val = desc.objectForKey(key.as_ref())?;
            // The dict value is documented as an NSNumber.  Cast via
            // raw pointer + NSNumber method dispatch — objc2's
            // generic downcast varies across versions, raw cast keeps
            // us API-stable.  SAFETY: documented NSDictionary value
            // type at the NSScreenNumber key.
            let ptr: *const objc2::runtime::AnyObject = &*val;
            let num: &NSNumber = &*(ptr as *const NSNumber);
            Some(num.unsignedIntValue())
        }
    }
}

/// Window-creation parameters.  Logical (point) coordinates.
#[derive(Clone, Debug)]
pub struct WindowAttrs {
    pub title: String,
    pub width_logical: f64,
    pub height_logical: f64,
    /// Window background (sRGB 0..1), painted from the first frame
    /// before any renderer attaches.  Shell passes terminal-black so
    /// a core swap never flashes a lighter band; standalone marspot /
    /// mcli pass the chrome panel tone.
    pub bg: (f64, f64, f64),
    /// When `Some`, the window opens at exactly this frame (screen
    /// points, bottom-left origin — `window_frame_pt`'s output)
    /// instead of the default `width/height_logical` at the system-
    /// chosen position.  Used by the shell self-update path so the
    /// exec'd successor doesn't jump the window.
    pub frame_pt: Option<(f64, f64, f64, f64)>,
}

/// Cross-thread wake signal.  Construct once before run_app starts;
/// clones are cheap (`Arc` bump) and `Send + Sync`.  `wake()` from
/// any thread schedules a `MarspotApp::user_event` on the main thread.
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
    // requests are flushed by `dispatch_event_for`.
    //
    // A wake is about the process, not a window — bytes arrived from
    // the core — so it goes to the first window, whose `user_event`
    // drains for everyone.
    dispatch_event_for(marspot_term::shell_proto::FIRST_WINDOW_ID, EventKind::UserEvent);
}

// ---------------------------------------------------------------------------
// Custom NSView subclass
// ---------------------------------------------------------------------------

/// Per-instance state for the custom view.
pub struct MarspotViewIvars {
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
    /// Focused-pane caret rect in **view-local physical pixels**
    /// (top-left origin, y-down — matches `isFlipped == true` view
    /// coords scaled by `backingScaleFactor`).  Written by the main
    /// loop after each render via `MarspotAppCtx::set_caret_rect_phys`;
    /// read by `firstRectForCharacterRange:` to anchor the IME
    /// candidate window under the caret instead of letting AppKit
    /// fall back to the screen-centre default.  `None` until the
    /// first render or when there's no visible caret.
    caret_view_phys_rect: Cell<Option<NSRect>>,
    /// RFC-005 — which window this view belongs to.  Every event the
    /// view raises is tagged with it, so a second window's clicks
    /// cannot be attributed to the first.
    window_id: Cell<u32>,
}

define_class!(
    /// `NSView` subclass that captures key + mouse + scroll events
    /// and bridges them into `MarspotApp`.  Implements
    /// `NSTextInputClient` so CJK / emoji IMEs can compose into the
    /// terminal.
    // SAFETY:
    // - Superclass NSView has no special subclassing requirements.
    // - MainThreadOnly is correct for an NSView subclass.
    // - Drop-relevant state in ivars is safe inside RefCell/Cell.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarspotView"]
    #[ivars = MarspotViewIvars]
    pub struct MarspotView;

    unsafe impl NSObjectProtocol for MarspotView {}

    impl MarspotView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // Default AppKit behaviour swallows the first mouse-down on a
        // non-key window — it only activates the window, never reaches
        // the view.  For a multi-pane terminal that means "click the
        // pane I want" requires two clicks when marspot is unfocused.
        // Returning YES here routes that first click straight into
        // mouse_down, so the window-activation and pane-focus switch
        // happen in the same gesture.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // Flip Y-axis so origin is top-left (matches the rest of the
        // code base's convention; layout / hit-testing assume top-left).
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let mods = nsevent_modifiers(event);

            // Cmd / Ctrl combos bypass the IME entirely.  This keeps
            // shortcuts that the rest of marspot expects (Cmd-V paste,
            // Ctrl-C → 0x03, Ctrl-[ → ESC) working — IMEs typically
            // don't consume these but we don't want to depend on that.
            if mods.super_ || mods.control {
                if let Some(ev) = nsevent_to_mars_key(event, KeyState::Pressed) {
                    dispatch_event_for(self.ivars().window_id.get(), EventKind::Key(ev, mods));
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
            { self.interpretKeyEvents(&array) };

            if !self.ivars().ime_consumed.get() {
                // Composing guard: when marked_text is non-empty the
                // IME owns the keyboard — even if it didn't call any
                // NSTextInputClient method this round.  Pinyin IME
                // hitting Space with no candidates is the canonical
                // case: it pockets the key without ever touching us.
                // Falling through to raw nsevent dispatch would leak
                // the ASCII byte (Space, BackTab, anything) into the
                // PTY and break composition.  Drop it on the floor;
                // IME will eventually unmarkText / insertText to
                // close the composition.
                if !self.ivars().marked_text.borrow().is_empty() {
                    return;
                }
                if let Some(ev) = nsevent_to_mars_key(event, KeyState::Pressed) {
                    dispatch_event_for(self.ivars().window_id.get(), EventKind::Key(ev, mods));
                }
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            // Released events bypass IME — IMEs only consume key-down.
            if let Some(ev) = nsevent_to_mars_key(event, KeyState::Released) {
                let mods = nsevent_modifiers(event);
                dispatch_event_for(self.ivars().window_id.get(), EventKind::Key(ev, mods));
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            // Modifiers carry on the event object; we surface them via
            // the next key_event delivery.  No callback to MarspotApp
            // here — modifiers ride along with the keystrokes that
            // actually arrive.
            let _ = event;
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            // locationInWindow is in window coords (logical points,
            // origin bottom-left of window).  Convert into our flipped
            // view's coords — with isFlipped=true that gives top-down
            // y in logical points.  Then scale to backing pixels by
            // hand: convertPointToBacking has under-documented Y-flip
            // behaviour on isFlipped views (the previous
            // `view_h + backing.y` workaround inverted top clicks
            // into bottom y_phys, sending sidebar entry "1" to row 7).
            let loc_window = { event.locationInWindow() };
            let loc_view = self.convertPoint_fromView(loc_window, None);
            let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let x_phys = loc_view.x * scale;
            let y_phys = loc_view.y * scale;
            // Modifier flags carry on the NSEvent — read them here so
            // mouse_down callers can branch on Option (alt) for
            // block-wise selection, Cmd for chord shortcuts, etc.,
            // without round-tripping through last_modifiers (which
            // only updates on keyDown).
            let mods = nsevent_modifiers(event);
            dispatch_event_for(self.ivars().window_id.get(), EventKind::MouseDown { x: x_phys, y: y_phys, mods });
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            // Same coord conversion as mouse_down.  AppKit folds
            // Ctrl-Left-Click into this path on macOS by default.
            let loc_window = { event.locationInWindow() };
            let loc_view = self.convertPoint_fromView(loc_window, None);
            let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let x_phys = loc_view.x * scale;
            let y_phys = loc_view.y * scale;
            let mods = nsevent_modifiers(event);
            dispatch_event_for(self.ivars().window_id.get(), EventKind::MouseRightDown { x: x_phys, y: y_phys, mods });
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            // Same coord conversion as mouse_down — AppKit only
            // sends mouseDragged: between a paired mouseDown:
            // and mouseUp:, so the app can rely on order.
            let loc_window = { event.locationInWindow() };
            let loc_view = self.convertPoint_fromView(loc_window, None);
            let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let x_phys = loc_view.x * scale;
            let y_phys = loc_view.y * scale;
            // RFC-006 — resolve the hovered window per drag tick so
            // the drop preview tracks the pointer live.  Entry-point
            // context, same as mouseUp:.
            let (hover_window_id, hover_x, hover_y) = marspot_pointer_in_window();
            dispatch_event_for(
                self.ivars().window_id.get(),
                EventKind::MouseDrag { x: x_phys, y: y_phys, hover_window_id, hover_x, hover_y },
            );
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let loc_window = { event.locationInWindow() };
            let loc_view = self.convertPoint_fromView(loc_window, None);
            let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let x_phys = loc_view.x * scale;
            let y_phys = loc_view.y * scale;
            // Which marspot window is the pointer over RIGHT NOW?
            // AppKit delivers the whole drag to the window that took
            // the press, so a pane dragged into another window still
            // reports its release here — this is the one place the
            // actual drop target can be resolved.  Computed in the
            // native callback (an entry point, no APP_STATE borrow
            // yet).  0 = not over any marspot window.
            let (drop_window_id, dwx, dwy) = marspot_pointer_in_window();
            // Window hit → that window's physical coords; no window →
            // raw screen points (they place the new window).
            let (drop_x, drop_y) = if drop_window_id != 0 {
                (dwx, dwy)
            } else {
                let sp = NSEvent::mouseLocation();
                (sp.x, sp.y)
            };
            dispatch_event_for(
                self.ivars().window_id.get(),
                EventKind::MouseUp { x: x_phys, y: y_phys, drop_window_id, drop_x, drop_y },
            );
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            // AppKit only delivers mouseMoved: when the window is key
            // AND setAcceptsMouseMovedEvents is true (set in
            // build_window).  Coords match mouse_down.
            let loc_window = { event.locationInWindow() };
            let loc_view = self.convertPoint_fromView(loc_window, None);
            let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let x_phys = loc_view.x * scale;
            let y_phys = loc_view.y * scale;
            dispatch_event_for(self.ivars().window_id.get(), EventKind::MouseMove { x: x_phys, y: y_phys });
        }

        // ── NSDraggingDestination ──
        // AppKit calls these on any view that registered drag types
        // (`registerForDraggedTypes` in `run_app`); no protocol
        // declaration is needed on the class.  Only file URLs are
        // registered, so a session that reaches performDragOperation
        // always has file paths to extract.

        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(
            &self,
            _info: &ProtocolObject<dyn NSDraggingInfo>,
        ) -> NSDragOperation {
            // Copy arrow (+) — communicates "this inserts the path",
            // never moves/deletes the dragged file.
            NSDragOperation::Copy
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag_operation(
            &self,
            _info: &ProtocolObject<dyn NSDraggingInfo>,
        ) -> bool {
            true
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(
            &self,
            info: &ProtocolObject<dyn NSDraggingInfo>,
        ) -> bool {
            // Resolve dropped file URLs → filesystem paths.  Going
            // through NSURL (rather than trimming the `file://`
            // prefix by hand) handles percent-encoding — CJK
            // filenames arrive percent-encoded in the URL string.
            let pb = { info.draggingPasteboard() };
            let mut paths: Vec<String> = Vec::new();
            if let Some(items) = { pb.pasteboardItems() } {
                for item in items.iter() {
                    let url_str =
                        match unsafe { item.stringForType(NSPasteboardTypeFileURL) } {
                            Some(s) => s,
                            None => continue,
                        };
                    let path = { NSURL::URLWithString(&url_str) }
                        .and_then(|u| { u.path() });
                    if let Some(p) = path {
                        paths.push(p.to_string());
                    }
                }
            }
            if paths.is_empty() {
                false
            } else {
                // Drop point → view-local physical px, same conversion
                // as mouse_down, so the receiver can pane-hit-test it.
                let loc_window = { info.draggingLocation() };
                let loc_view = self.convertPoint_fromView(loc_window, None);
                let scale =
                    self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
                dispatch_event_for(self.ivars().window_id.get(), EventKind::FileDrop {
                    x: loc_view.x * scale,
                    y: loc_view.y * scale,
                    paths,
                });
                true
            }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // hasPreciseScrollingDeltas distinguishes trackpads
            // (pixel-precise) from wheels (line-stepped).  Scale on
            // the caller side via cell height.
            let precise = { event.hasPreciseScrollingDeltas() };
            let dx;
            let dy;
            if precise {
                // In points; convert to backing pixels.
                let scale =
                    self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
                dx = { event.scrollingDeltaX() } * scale;
                dy = { event.scrollingDeltaY() } * scale;
            } else {
                // Lines; pass through, caller scales.
                dx = event.deltaX();
                dy = event.deltaY();
            }
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Scroll { dx, dy, precise });
        }
    }

    unsafe impl NSTextInputClient for MarspotView {
        // Required: queries

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            !self.ivars().marked_text.borrow().is_empty()
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let len = self.ivars().marked_text.borrow().len();
            if len > 0 {
                NSRange::new(0, len as NSUInteger)
            } else {
                NSRange::new(NSNotFound as NSUInteger, 0)
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            // We don't maintain a selection model.  NSNotFound is
            // documented to mean "no selection".
            NSRange::new(NSNotFound as NSUInteger, 0)
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes_for_marked_text(
            &self,
        ) -> Retained<NSArray<NSAttributedStringKey>> {
            // Empty → IME defaults to plain text only.  We don't
            // honour underline / colour styling on preedit anyway.
            NSArray::new()
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring_for_proposed_range(
            &self,
            _range: NSRange,
            _actual_range: *mut NSRange,
        ) -> Option<Retained<NSAttributedString>> {
            // We don't expose buffered terminal text to the IME.
            None
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            0
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect_for_character_range(
            &self,
            _range: NSRange,
            _actual_range: *mut NSRange,
        ) -> NSRect {
            // AppKit invokes this to anchor the IME candidate window.
            // Expected return: screen-space points (bottom-left origin
            // per the standard NSScreen coordinate system).  Main loop
            // stashes the focused caret's rect in view-local physical
            // pixels (top-left, y-down — view.isFlipped == true) after
            // every render.
            //
            // We bypass `convertRect:toView:` on the flipped view —
            // its handling of `size.height` under isFlipped has bitten
            // us in practice (candidate window jumped to the top-left
            // of the screen).  Instead: scale phys → points in view,
            // flip y manually with `bounds.height`, add the view's
            // frame origin to land in the window's non-flipped
            // coordinate system, then go to screen.
            let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
            let phys = match self.ivars().caret_view_phys_rect.get() {
                Some(r) => r,
                None => return zero,
            };
            let self_view: &NSView = unsafe {
                &*(self as *const MarspotView as *const NSView)
            };
            // Manual conversion — `convertRectFromBacking` observed to
            // produce nonsensical values for our flipped layer-backed
            // view (returning y = -(phys.y + phys.h), unrelated to either
            // backing-y-up or view-y-down semantics). Bypass it: caller
            // gives us physical pixels in view-local top-left y-down, we
            // divide by backing scale and flip Y manually.
            let window = match self_view.window() {
                Some(w) => w,
                None => return zero,
            };
            let scale = window.backingScaleFactor();
            let bounds = self_view.bounds();
            // Backing px → view points (top-left, y-down).
            let pt_x = phys.origin.x / scale;
            let pt_y_top = phys.origin.y / scale;
            let pt_w = phys.size.width / scale;
            let pt_h = phys.size.height / scale;
            // Flip into view-local y-up (bottom-left origin).
            let pt_y_botup = bounds.size.height - pt_y_top - pt_h;
            // View frame is in the window's contentView (non-flipped)
            // coordinate system — add to land in window coords.
            let frame = self_view.frame();
            let window_rect = NSRect::new(
                NSPoint::new(frame.origin.x + pt_x, frame.origin.y + pt_y_botup),
                NSSize::new(pt_w, pt_h),
            );
            window.convertRectToScreen(window_rect)
        }

        // Required: composition lifecycle

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(
            &self,
            string: &NSObject,
            _selected_range: NSRange,
            _replacement_range: NSRange,
        ) {
            self.ivars().ime_consumed.set(true);
            let s = nsobject_string_to_string(string);
            *self.ivars().marked_text.borrow_mut() = s.clone();
            // Forward to the app so it can paint an inline preedit
            // overlay (pinyin candidates, hiragana composition, etc.).
            dispatch_event_for(self.ivars().window_id.get(), EventKind::ImePreedit(s));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.ivars().marked_text.borrow_mut().clear();
            dispatch_event_for(self.ivars().window_id.get(), EventKind::ImePreedit(String::new()));
        }

        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &NSObject, _replacement_range: NSRange) {
            self.ivars().ime_consumed.set(true);
            let s = nsobject_string_to_string(string);
            self.ivars().marked_text.borrow_mut().clear();
            // Commit ends the composition — clear any preedit overlay
            // before we send the committed text so the app doesn't
            // briefly render both.
            dispatch_event_for(self.ivars().window_id.get(), EventKind::ImePreedit(String::new()));
            if s.is_empty() {
                return;
            }
            // Treat IME-committed text as a single key-event with
            // logical=Other and the resolved text payload.  The
            // text fallback in input::key_event_to_bytes ships it
            // straight to the PTY, regardless of byte width.
            let ev = MarspotKeyEvent {
                state: KeyState::Pressed,
                logical: LogicalKey::Other,
                text: Some(s),
            };
            let mods = self.ivars().last_modifiers.get();
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Key(ev, mods));
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, selector: Sel) {
            // `interpretKeyEvents` lands here whenever the IME handed
            // the key event off as a standard editing command —
            // Enter, Tab, Backspace, arrows, etc.  In every case the
            // IME has CONSUMED the key, even when the selector is
            // `noop:` ("I dropped it on the floor"), so always mark
            // ime_consumed=true and let the keyDown tail skip the
            // raw fallback.
            self.ivars().ime_consumed.set(true);
            // Composition guard: while marked text is non-empty the
            // IME owns navigation + commit too.  Arrow keys move the
            // candidate cursor, Enter commits, Backspace edits the
            // preedit — none of them should reach the PTY.  Without
            // this guard, hitting Right while composing translated
            // to `\x1b[C` and the user saw "[C[C[C" smear into the
            // claudecode input field.  Same shape as the Space-leak
            // we fixed in the raw fallback path; the IME just routes
            // arrows here instead of pocketing them silently.
            if !self.ivars().marked_text.borrow().is_empty() {
                return;
            }
            let Some(named) = selector_named_key(selector) else {
                return;
            };
            let ev = MarspotKeyEvent {
                state: KeyState::Pressed,
                logical: LogicalKey::Named(named),
                text: None,
            };
            let mods = self.ivars().last_modifiers.get();
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Key(ev, mods));
        }
    }
);

/// Map a Cocoa `NSStandardKeyBindingResponding` selector — fired by
/// `interpretKeyEvents` for non-text key presses — to our NamedKey
/// enum.  Returns None for selectors we don't translate (typically
/// `noop:`, fired for ctrl-letter combos that we already routed
/// through the raw path before entering IME).
fn selector_named_key(selector: Sel) -> Option<NamedKey> {
    // objc2 0.6: `Sel::name()` returns `&CStr`.  Selector names are
    // always ASCII, so the lossy fallback never actually fires.
    match selector.name().to_str().unwrap_or("") {
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
    if let Some(attr) = string.downcast_ref::<NSAttributedString>() {
        attr.string().to_string()
    } else {
        let p: *const NSObject = string;
        let p: *const NSString = p.cast();
        unsafe { (*p).to_string() }
    }
}

// ---------------------------------------------------------------------------
// NSWindowDelegate subclass
// ---------------------------------------------------------------------------

pub struct MarspotWindowDelegateIvars {
    /// RFC-005 — the window this delegate speaks for.
    window_id: Cell<u32>,
}

define_class!(
    // SAFETY:
    // - Superclass NSObject has no subclassing requirements.
    // - Window delegates are main-thread-only.
    // - No Drop logic here.
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarspotWindowDelegate"]
    #[ivars = MarspotWindowDelegateIvars]
    pub struct MarspotWindowDelegate;

    unsafe impl NSObjectProtocol for MarspotWindowDelegate {}

    unsafe impl NSWindowDelegate for MarspotWindowDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            dispatch_event_for(self.ivars().window_id.get(), EventKind::CloseRequested);
            // Returning false lets the app handle the close; if the
            // app calls ctx.exit() inside close_requested, the run
            // loop stops and we return.  Marspot's current behaviour is
            // "close = exit", but we honour the indirection cleanly.
            false
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Resized);
        }

        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, _notification: &NSNotification) {
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Moved);
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Focused(true));
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            dispatch_event_for(self.ivars().window_id.get(), EventKind::Focused(false));
        }
    }

    unsafe impl NSApplicationDelegate for MarspotWindowDelegate {
        /// RFC-003 §6 Amendment 15 — route Cmd-Q / Quit Marspot menu /
        /// dock Quit through the same `CloseRequested` path as the
        /// window's red close button.  Returning `NSTerminateCancel`
        /// stops AppKit's own teardown (which would deadlock against
        /// `windowShouldClose:`'s `false` return below) and lets our
        /// `close_requested` handler shut down cleanly, then call
        /// `ctx.exit()` so the run loop drains naturally.  Without
        /// this redirect AppKit's `terminate:` runs straight to
        /// `exit(0)` and the L1 cleanup (SIGTERM L3s + drain
        /// fd-vault + pgrep sweep) never gets a chance to fire —
        /// that's the 2026-06-17 "user Cmd-Q'd but the 9 L3s lived
        /// on" surprise.
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _sender: &NSApplication,
        ) -> NSApplicationTerminateReply {
            dispatch_event_for(self.ivars().window_id.get(), EventKind::CloseRequested);
            NSApplicationTerminateReply::TerminateCancel
        }
    }
);

// ---------------------------------------------------------------------------
// Event dispatch
// ---------------------------------------------------------------------------

pub enum EventKind {
    UserEvent,
    Key(MarspotKeyEvent, Modifiers),
    MouseDown { x: f64, y: f64, mods: Modifiers },
    MouseRightDown { x: f64, y: f64, mods: Modifiers },
    ImePreedit(String),
    MouseDrag { x: f64, y: f64, hover_window_id: u32, hover_x: f64, hover_y: f64 },
    MouseUp { x: f64, y: f64, drop_window_id: u32, drop_x: f64, drop_y: f64 },
    MouseMove { x: f64, y: f64 },
    Scroll { dx: f64, dy: f64, precise: bool },
    /// Finder file drop on the view.  `(x, y)` is the drop point in
    /// physical px (top-left origin, same as MouseDown); `paths` are
    /// the resolved filesystem paths (≥1 — empty drops are rejected
    /// in performDragOperation).
    FileDrop { x: f64, y: f64, paths: Vec<String> },
    Resized,
    Moved,
    Focused(bool),
    CloseRequested,
    /// RFC-005 — a window the app asked for now exists.  Delivered with
    /// that window's own ctx, which is what makes it safe to size its
    /// IOSurface pair here: a window on another display can have a
    /// different backing scale from the key one.
    WindowOpened,
    /// Dev panel NSWindow was resized / moved / changed visibility.
    /// Signals that the next redraw needs to recompute dev panel
    /// dimensions and re-paint into the new layer size.
    DevWindowChanged,
    /// User clicked inside the dev panel NSWindow's content area.
    /// Coords are logical pt in view-local (`isFlipped` so y=0 at top).
    /// L1 hit-tests against the dev panel layout to update active
    /// tab / section, then re-draws.
    DevPanelClick { x_pt: f64, y_pt: f64 },
    /// Trackpad / scroll wheel inside the dev panel.  Already
    /// sign-inverted at the source: positive = content scrolls down
    /// (offset_y increases).  In logical pt; multiply by current
    /// scale at apply time to get phys.
    DevPanelScroll { delta_y_pt: f64 },
}

struct AppState {
    app: Box<dyn MarspotApp>,
    /// One context per open window, in creation order.  Never empty
    /// while the app is running.
    windows: Vec<MarspotAppCtx>,
}

impl AppState {
    fn window_index(&self, window_id: u32) -> Option<usize> {
        self.windows.iter().position(|c| c.window_id == window_id)
    }
}

/// A window lifecycle request raised from inside the `APP_STATE`
/// borrow, to be applied once that borrow is gone.
enum WindowOp {
    Open(u32, WindowAttrs),
    Close(u32),
    /// Dev/test only — see `perform_close`.
    PerformClose(u32),
    /// Caret rect for a window that is not the one currently
    /// dispatching — see `set_caret_rect_phys_for`.
    SetCaretRect(u32, Option<(f64, f64, f64, f64)>),
}

thread_local! {
    /// Owns the user's `MarspotApp` and the per-window context.
    /// Populated by `run_app`; accessed from every event handler.
    static APP_STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };

    /// Window opens / closes queued from inside a dispatch.
    ///
    /// Every event handler runs while `APP_STATE` is mutably borrowed,
    /// so touching it again from a handler is an instant
    /// "RefCell already borrowed" panic — and `makeKeyAndOrderFront:` /
    /// `close` make it worse by synchronously firing delegate
    /// notifications straight back into `dispatch_event_for`.  Same
    /// hazard, same remedy as the dev window's deferred actions.
    static PENDING_WINDOW_OPS: RefCell<Vec<WindowOp>> =
        const { RefCell::new(Vec::new()) };

    /// How many windows are open, readable without borrowing
    /// `APP_STATE` — handlers need this while that borrow is live.
    static WINDOW_COUNT: Cell<usize> = const { Cell::new(0) };
}

/// Dispatch an event that is about the app rather than a particular
/// window — the dev panel's own NSWindow raises these.  Routed to the
/// first window, which is the one the dev panel is anchored against.
pub(crate) fn dispatch_event_pub(kind: EventKind) {
    dispatch_event_for(marspot_term::shell_proto::FIRST_WINDOW_ID, kind);
}

fn dispatch_event_for(window_id: u32, kind: EventKind) {
    APP_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else { return };
        // A window that has already been torn down can still have an
        // event in flight from AppKit; dropping it is correct, and is
        // not an error worth logging on a hot path.
        //
        // Except a wake, which is about the *process* — bytes arrived
        // from the core — and merely addressed to the boot window by
        // convention (`source_perform`).  Dropping those when the boot
        // window is the one that closed stops the app draining the
        // core at all: the windows still open freeze on their last
        // frame, and the supervisor stops ticking.  Any live window
        // can carry it, so use whichever is still here.
        let i = match state.window_index(window_id) {
            Some(i) => i,
            None if matches!(kind, EventKind::UserEvent) && !state.windows.is_empty() => 0,
            None => return,
        };
        let AppState { app, windows } = state;
        let ctx = &mut windows[i];

        match kind {
            EventKind::UserEvent => app.user_event(ctx),
            EventKind::Key(ev, mods) => app.key_event(ctx, ev, mods),
            EventKind::MouseDown { x, y, mods } => app.mouse_down(ctx, x, y, mods),
            EventKind::MouseRightDown { x, y, mods } => app.mouse_right_down(ctx, x, y, mods),
            EventKind::ImePreedit(text) => app.ime_preedit_changed(ctx, &text),
            EventKind::MouseDrag { x, y, hover_window_id, hover_x, hover_y } => {
                app.mouse_drag(ctx, x, y, hover_window_id, hover_x, hover_y)
            }
            EventKind::MouseUp { x, y, drop_window_id, drop_x, drop_y } => {
                app.mouse_up(ctx, x, y, drop_window_id, drop_x, drop_y)
            }
            EventKind::MouseMove { x, y } => app.mouse_moved(ctx, x, y),
            EventKind::Scroll { dx, dy, precise } => app.scroll(ctx, dx, dy, precise),
            EventKind::FileDrop { x, y, paths } => app.file_drop(ctx, x, y, &paths),
            EventKind::Resized => {
                let (w, h) = ctx.inner_size_phys();
                app.resized(ctx, w, h);
            }
            EventKind::Moved => app.moved(ctx),
            EventKind::Focused(f) => app.focused(ctx, f),
            EventKind::CloseRequested => app.close_requested(ctx),
            EventKind::WindowOpened => app.window_opened(ctx),
            EventKind::DevWindowChanged => {
                // The dev window's NSWindowDelegate noticed a resize
                // / move / etc.  Hand the event to the App impl so
                // L1 can persist the new geometry, then drive a
                // redraw so the dev panel recomputes against the
                // new content area.
                app.dev_window_changed(ctx);
                ctx.request_redraw();
            }
            EventKind::DevPanelClick { x_pt, y_pt } => {
                app.dev_panel_click(ctx, x_pt, y_pt);
                ctx.request_redraw();
            }
            EventKind::DevPanelScroll { delta_y_pt } => {
                app.dev_panel_scroll(ctx, delta_y_pt);
                ctx.request_redraw();
            }
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
    // Apply any AppKit window operations queued from inside the
    // APP_STATE borrow (dev panel show/hide).  Running here means
    // `makeKeyAndOrderFront:` can synchronously fire window
    // notifications back through `dispatch_event` without
    // tripping a re-entrant `borrow_mut`.
    crate::dev_window::drain_pending_actions();
    drain_pending_windows();
}

fn post_dummy_event(nsapp: &NSApplication) {
    // Construct a no-op application-defined event so NSApp.run sees
    // an event after stop() and returns.  Type 15 is NSEventTypeApplicationDefined.
    use objc2::class;
    use objc2::msg_send;
    unsafe {
        let event_cls = class!(NSEvent);
        let nsevent: *mut NSEvent = msg_send![event_cls,
            otherEventWithType: 15u64 /* NSEventTypeApplicationDefined */,
            location: NSPoint::new(0.0, 0.0),
            modifierFlags: 0u64,
            timestamp: 0.0_f64,
            windowNumber: 0_isize,
            context: ptr::null::<c_void>(),
            subtype: 0_i16,
            data1: 0_isize,
            data2: 0_isize,
        ];
        if !nsevent.is_null() {
            let nsevent = &*nsevent;
            nsapp.postEvent_atStart(nsevent, true);
        }
    }
}

/// Read `Resources/AppIcon.icns` off our bundle and force the Dock /
/// app-switcher to redraw with it.  macOS otherwise caches the icon
/// at first-launch and won't pick up post-install changes until cold
/// re-launch — for marspot's silent-update + self-execv design that
/// means the user sees stale icons after every brand refresh.  Reading
/// the .icns from disk every startup is cheap (~74 KB) and the install
/// path keeps it in lockstep with the codebase.
fn set_dock_icon_from_bundle(nsapp: &NSApplication) {
    use objc2::rc::Retained;
    use std::path::PathBuf;
    // Try multiple candidate paths.  First-launch we run as
    // `<bundle>/Contents/MacOS/marspot-shell` and 2 parents up resolves
    // to `<bundle>/Contents/`.  After L1 self-execv we run as
    // `~/Library/Caches/marspot/binaries/current/marspot-shell` and
    // that walk leads to `~/Library/Caches/marspot/binaries/` — no
    // Resources there;  fall through to the known install path so
    // post-execv L1 still updates the Dock.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(macos_dir) = exe.parent() {
            if let Some(contents) = macos_dir.parent() {
                candidates.push(contents.join("Resources").join("AppIcon.icns"));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local/Marspot.app/Contents/Resources/AppIcon.icns"),
        );
    }
    let icon_path = match candidates.into_iter().find(|p| p.exists()) {
        Some(p) => p,
        None => return,
    };
    let path_str = match icon_path.to_str() { Some(s) => s, None => return };
    let ns_path = NSString::from_str(path_str);
    let img: Option<Retained<NSImage>> = {
        NSImage::initWithContentsOfFile(NSImage::alloc(), &ns_path)
    };
    if let Some(img) = img {
        unsafe { nsapp.setApplicationIconImage(Some(&img)) };
    }
}

// ---------------------------------------------------------------------------
// run_app
// ---------------------------------------------------------------------------

/// Build one native window: view, NSWindow, delegate — wired together
/// and ready to show.
///
/// Extracted so `run_app` (boot window) and `open_window` (every
/// window after it) cannot drift apart.  AppKit setup that differs
/// between the two by accident is exactly the kind of bug that only
/// shows up in the second window.
fn build_window(
    mtm: MainThreadMarker,
    window_id: u32,
    attrs: &WindowAttrs,
) -> (
    Retained<NSWindow>,
    Retained<MarspotView>,
    Retained<MarspotWindowDelegate>,
) {
    // 1. Create custom NSView (origin top-left) sized to logical attrs.
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(attrs.width_logical, attrs.height_logical),
    );
    let view: Retained<MarspotView> = {
        let alloc = MarspotView::alloc(mtm).set_ivars(MarspotViewIvars {
            marked_text: RefCell::new(String::new()),
            ime_consumed: Cell::new(false),
            last_modifiers: Cell::new(Modifiers::default()),
            caret_view_phys_rect: Cell::new(None),
            window_id: Cell::new(window_id),
        });
        unsafe { msg_send![super(alloc), initWithFrame: frame] }
    };
    // Accept Finder file drags anywhere on the view — dropping a
    // file inserts its shell-quoted path into the pane under the
    // cursor (NSDraggingDestination methods on MarspotView).
    // (`from_id_slice` + `copy` because NSString's mutable-subclass
    // mutability blocks the plain `from_slice` retainable bound;
    // copying an immutable NSString is just a retain.)
    let drag_types = NSArray::from_retained_slice(&[unsafe { NSPasteboardTypeFileURL.copy() }]);
    { view.registerForDraggedTypes(&drag_types) };

    // 2. Create NSWindow with view as content.
    //
    // FullSizeContentView + titlebarAppearsTransparent fuse the title
    // bar into the window content: the system traffic-light buttons
    // float over the topmost row of the content, no separate
    // chrome-grey strip cuts the working area off from the window
    // edge.  Window backgroundColor matches the cell BG so the
    // overlap is invisible — the user sees a single dark surface
    // with the buttons floating in the corner.
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    let window: Retained<NSWindow> = unsafe {
        let alloc = NSWindow::alloc(mtm);
        msg_send![alloc,
            initWithContentRect: frame,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    // We hold this window in a `Retained` for as long as it is open,
    // so AppKit must NOT also release it on close.  A window built
    // programmatically (rather than loaded from a nib) defaults to
    // `releasedWhenClosed == YES`: `close()` drops AppKit's reference
    // and our `Retained` then drops another, over-releasing it.  The
    // window is freed while the close animation still holds it, and
    // the app dies with SIGSEGV inside
    // `-[_NSWindowTransformAnimation dealloc]` — which is exactly
    // what closing a second window did (2026-07-28).  The boot window
    // never showed it: closing that one quits the app, so nothing
    // afterwards touched the freed object.
    // SAFETY: plain setter on a window we just created and still own
    // exclusively; nothing else can observe it mid-call.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&attrs.title));
    {
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        // Kill the 1-px hairline AppKit draws under the titlebar
        // (the visible darker strip the user kept seeing even after
        // FullSizeContentView).  `NSTitlebarSeparatorStyleNone`
        // is the macOS 11+ knob for "no separator at all".
        window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
        // Window BG is caller-chosen so the very first paint (before
        // any renderer/presenter attaches) is already the right tone
        // — no flash of a default colour.  The shell (L1) passes its
        // terminal-black so a core (L2) swap that briefly uncovers
        // the layer shows steady black, not a lighter chrome band.
        let (r, g, b) = attrs.bg;
        let bg = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
        window.setBackgroundColor(Some(&bg));
    }
    window.setContentView(Some(unsafe {
        &*(Retained::as_ptr(&view) as *const NSView)
    }));
    // Mouse-moved delivery: enabled so chrome hover affordances
    // (icon button BG darkens under cursor) update without a click.
    // High-frequency but the dispatch path is cheap: NSView →
    // EventKind::MouseMove → MarspotApp::mouse_moved (default no-op).
    window.setAcceptsMouseMovedEvents(true);
    window.makeFirstResponder(Some(&view));

    // Window delegate.
    let delegate: Retained<MarspotWindowDelegate> = {
        let alloc = MarspotWindowDelegate::alloc(mtm)
            .set_ivars(MarspotWindowDelegateIvars {
                window_id: Cell::new(window_id),
            });
        unsafe { msg_send![super(alloc), init] }
    };
    let proto: &ProtocolObject<dyn NSWindowDelegate> =
        ProtocolObject::from_ref(&*delegate);
    window.setDelegate(Some(proto));

    (window, view, delegate)
}

/// Open an additional native window.
///
/// Queued, not immediate: callers are event handlers, and an event
/// handler runs inside the `APP_STATE` borrow.  The window appears
/// when `drain_pending_windows` runs at the end of the dispatch, and
/// the app is told through `MarspotApp::window_opened` — which is
/// where it can size surfaces against the real window.
pub fn open_window(window_id: u32, attrs: &WindowAttrs) {
    PENDING_WINDOW_OPS
        .with(|q| q.borrow_mut().push(WindowOp::Open(window_id, attrs.clone())));
}

/// Ask AppKit to close the window the way the red button does —
/// `performClose:`, which runs the delegate's `windowShouldClose:`
/// and, through it, the app's own close path.
///
/// Dev/test only.  It exists because that re-entrant shape (AppKit is
/// inside its close machinery while our handler closes and releases
/// the window) is what a script cannot otherwise reproduce, and it is
/// exactly the shape that crashed on 2026-07-28.  Queued for the same
/// reason as the others.
pub fn perform_close(window_id: u32) {
    PENDING_WINDOW_OPS
        .with(|q| q.borrow_mut().push(WindowOp::PerformClose(window_id)));
}

/// Publish a caret rect to the view of `window_id`, whichever window
/// is currently dispatching.
///
/// `MarspotAppCtx::set_caret_rect_phys` can only reach the window it
/// was handed, and the caret stream does not arrive on that window's
/// context: it rides the core's control socket, which is drained in
/// `user_event` — an event about the *process*, so always dispatched
/// against the first window.  With one window the two coincided; with
/// two, every window's caret landed on window 1's view.  The second
/// window then answered `firstRectForCharacterRange:` with a zero
/// rect and macOS parked the IME candidate window off in a corner of
/// the screen instead of under the caret.
///
/// Queued for the same reason as `open_window`: callers are event
/// handlers, and `APP_STATE` is mutably borrowed for the whole of one.
/// The drain runs in the same dispatch's tail, so the view is up to
/// date well before AppKit next asks it where the caret is.
pub fn set_caret_rect_phys_for(window_id: u32, rect: Option<(f64, f64, f64, f64)>) {
    PENDING_WINDOW_OPS
        .with(|q| q.borrow_mut().push(WindowOp::SetCaretRect(window_id, rect)));
}

/// Tear down the window carrying `window_id`.  Queued, for the same
/// reason as `open_window`.
pub fn close_window(window_id: u32) {
    PENDING_WINDOW_OPS.with(|q| q.borrow_mut().push(WindowOp::Close(window_id)));
}

/// Apply queued window opens / closes.
///
/// Runs after the `APP_STATE` borrow is released, so the AppKit calls
/// here are free to fire delegate notifications back through
/// `dispatch_event_for`.
fn drain_pending_windows() {
    loop {
        let op = PENDING_WINDOW_OPS.with(|q| {
            let mut q = q.borrow_mut();
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        });
        let Some(op) = op else { break };
        match op {
            WindowOp::Open(window_id, attrs) => {
                let Some(mtm) = MainThreadMarker::new() else { continue };
                let nsapp = NSApplication::sharedApplication(mtm);
                let (window, view, delegate) = build_window(mtm, window_id, &attrs);
                if let Some((x, y, w, h)) = attrs.frame_pt {
                    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
                    window.setFrame_display(rect, false);
                } else {
                    // Centre rather than land exactly on top of the
                    // window this was opened from.
                    window.center();
                }
                window.makeKeyAndOrderFront(None);
                let ctx = MarspotAppCtx {
                    window_id,
                    inner: view,
                    nswindow: window,
                    nsapp,
                    _delegate: delegate,
                    redraw_pending: Cell::new(false),
                    exit_requested: Cell::new(false),
                };
                let added = APP_STATE.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    match slot.as_mut() {
                        Some(state) => {
                            state.windows.push(ctx);
                            WINDOW_COUNT.with(|c| c.set(state.windows.len()));
                            true
                        }
                        None => false,
                    }
                });
                if added {
                    // Tell the app its window exists.  It needs a real
                    // window to size surfaces against, and the ctx it
                    // gets here is that window's own — not the key
                    // one, whose backing scale may differ.
                    dispatch_event_for(window_id, EventKind::WindowOpened);
                }
            }
            WindowOp::PerformClose(window_id) => {
                let win = APP_STATE.with(|cell| {
                    let slot = cell.borrow();
                    let state = slot.as_ref()?;
                    let i = state.window_index(window_id)?;
                    Some(state.windows[i].nswindow.clone())
                });
                if let Some(w) = win {
                    // Key, and the app active: closing the KEY window
                    // is what makes AppKit hand focus on with an
                    // ordering animation, and that animation is what
                    // the 2026-07-28 crash died inside.
                    if let Some(mtm) = MainThreadMarker::new() {
                        let nsapp = NSApplication::sharedApplication(mtm);
                        #[allow(deprecated)]
                        nsapp.activateIgnoringOtherApps(true);
                    }
                    w.makeKeyAndOrderFront(None);
                    w.performClose(None);
                }
            }
            WindowOp::SetCaretRect(window_id, rect) => {
                APP_STATE.with(|cell| {
                    let slot = cell.borrow();
                    let Some(state) = slot.as_ref() else { return };
                    // A caret can still be in flight for a window the
                    // user just closed; dropping it is correct.
                    let Some(i) = state.window_index(window_id) else { return };
                    let ctx = &state.windows[i];
                    ctx.set_caret_rect_phys(rect);
                    caret_probe(ctx, rect);
                });
            }
            WindowOp::Close(window_id) => {
                let closing = APP_STATE.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    let state = slot.as_mut()?;
                    let i = state.window_index(window_id)?;
                    let ctx = state.windows.remove(i);
                    WINDOW_COUNT.with(|c| c.set(state.windows.len()));
                    Some(ctx)
                });
                if let Some(ctx) = closing {
                    // Delegate off first: `close` fires notifications,
                    // and this window's state is already gone.
                    ctx.nswindow.setDelegate(None);
                    ctx.nswindow.close();
                }
            }
        }
    }
}

/// Dev seam (`MARSPOT_DEV_CARET_PROBE=1`) — print where the IME
/// candidate window would be anchored for `ctx`, asking the view the
/// same question AppKit asks it.
///
/// A script cannot see an IME candidate window, so this is how
/// `bin/test-multi-window.sh` proves each window anchors the candidate
/// box against its OWN caret: the screen rect printed here has to fall
/// inside that window's own frame.  Off unless the env var is set —
/// it runs one Objective-C round-trip per caret move.
fn caret_probe(ctx: &MarspotAppCtx, rect: Option<(f64, f64, f64, f64)>) {
    // Read the env once: a caret lands at most once per rendered
    // frame, and `env::var_os` locks the environment and scans it.
    thread_local! {
        static ON: bool = std::env::var_os("MARSPOT_DEV_CARET_PROBE").is_some();
    }
    if !ON.with(|on| *on) {
        return;
    }
    // SAFETY: our own NSTextInputClient method on our own view, on the
    // main thread; a null `actualRange` is what AppKit itself passes
    // when it does not want the resolved range back.
    let screen: NSRect = unsafe {
        ctx.inner.firstRectForCharacterRange_actualRange(
            NSRange::new(0, 0),
            std::ptr::null_mut(),
        )
    };
    let f = ctx.nswindow.frame();
    eprintln!(
        "[marspot] ime.caret window_id={} phys={} screen={:.1},{:.1},{:.1},{:.1} \
         window={:.1},{:.1},{:.1},{:.1}",
        ctx.window_id,
        rect.is_some(),
        screen.origin.x,
        screen.origin.y,
        screen.size.width,
        screen.size.height,
        f.origin.x,
        f.origin.y,
        f.size.width,
        f.size.height,
    );
}

/// The pointer resolved into a marspot window's own space:
/// `(window_id, x_phys, y_phys)` in that window's flipped view
/// coordinates, or `(0, 0, 0)` when the pointer is not over any
/// marspot window.  RFC-006's drop preview is drawn from this during
/// a drag.  Entry-point context only (borrows `APP_STATE`).
fn marspot_pointer_in_window() -> (u32, f64, f64) {
    let Some(mtm) = MainThreadMarker::new() else { return (0, 0.0, 0.0) };
    let point = NSEvent::mouseLocation();
    let number =
        NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(point, 0, mtm);
    APP_STATE.with(|cell| {
        let slot = cell.borrow();
        let Some(state) = slot.as_ref() else { return (0, 0.0, 0.0) };
        let Some(c) = state
            .windows
            .iter()
            .find(|c| c.nswindow.windowNumber() == number)
        else {
            return (0, 0.0, 0.0);
        };
        let win_pt = c.nswindow.convertPointFromScreen(point);
        let view_pt = c.inner.convertPoint_fromView(win_pt, None);
        let scale = c.nswindow.backingScaleFactor();
        (c.window_id, view_pt.x * scale, view_pt.y * scale)
    })
}

/// How many windows are open.
///
/// Reads a mirror rather than `APP_STATE`, so it is safe to call from
/// an event handler (which runs inside that borrow).
pub fn window_count() -> usize {
    WINDOW_COUNT.with(|c| c.get())
}

/// Block on the AppKit run loop, dispatching events into `app`.
/// Returns when `ctx.exit()` has been called and the run loop drains.
pub fn run_app<A: MarspotApp>(app: A, proxy: EventProxy, attrs: WindowAttrs) {
    let mtm = MainThreadMarker::new()
        .expect("run_app must be called on the main thread");
    // The boot window's id.  RFC-005 step 4 adds `open_window` for the
    // rest; they take ids from the same monotonic source.
    let window_id = marspot_term::shell_proto::FIRST_WINDOW_ID;
    let nsapp = NSApplication::sharedApplication(mtm);
    nsapp.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // Force-refresh the Dock icon from the bundle's `Resources/AppIcon
    // .icns`.  Without this, a running marspot session shows whichever
    // icon was cached at first launch — even after `bin/install-local
    // .sh` lands a new .icns, the live Dock still draws the old one
    // until next cold launch.  Reading from disk each startup is cheap
    // (~74 KB) and self-update keeps the icon in lockstep with the
    // codebase.
    set_dock_icon_from_bundle(&nsapp);

    let (window, view, delegate) = build_window(mtm, window_id, &attrs);
    // RFC-003 §6 Amendment 15 — the boot window's delegate also wears
    // the NSApplicationDelegate hat so Cmd-Q / Quit menu / dock Quit
    // run through `close_requested`.  Only one object can hold that
    // role, so later windows never take it.
    {
        let app_proto: &ProtocolObject<dyn NSApplicationDelegate> =
            ProtocolObject::from_ref(&*delegate);
        nsapp.setDelegate(Some(app_proto));
    }

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
    let ctx = MarspotAppCtx {
        window_id,
        inner: view.clone(),
        nswindow: window.clone(),
        nsapp: nsapp.clone(),
        _delegate: delegate.clone(),
        redraw_pending: Cell::new(false),
        exit_requested: Cell::new(false),
    };

    // Restore an exact predecessor frame BEFORE first show, so the
    // self-updated shell's window appears in place rather than
    // opening at the default rect and visibly jumping.
    if let Some((x, y, w, h)) = attrs.frame_pt {
        let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        window.setFrame_display(rect, false);
    } else {
        // No saved frame — a first launch, or the one after the user
        // closed everything.  `initWithContentRect` put the window at
        // the screen's bottom-left corner; centre it, the same as any
        // window opened later without a frame.
        window.center();
    }

    // Show + focus + activate.  `activate()` replaces the deprecated
    // `activateIgnoringOtherApps(true)`; macOS 14+ is our target floor.
    { nsapp.activate() };
    window.makeKeyAndOrderFront(None);

    // Hand control to the app's resumed handler.  Borrow the cell as
    // `Box<dyn MarspotApp>` so `dispatch_event` and resumed share state.
    APP_STATE.with(|cell| {
        *cell.borrow_mut() = Some(AppState {
            app: Box::new(app),
            windows: vec![ctx],
        });
    });

    // Build the independent dev-panel window BEFORE handing control
    // to the app's `resumed` handler — `resumed` is where L1 reads
    // `dev-window-state.bin` and calls `apply_saved_frame`, which
    // needs `with_dev_window` to actually find a built window.
    // Reverse order silently drops the saved frame (real bug observed
    // 2026-06-23: dev panel always opened at default 420×552 after
    // every install/restart, ignoring the persisted geometry).
    if let Err(e) = crate::dev_window::ensure_built(mtm) {
        eprintln!("[marspot] dev_window init failed: {e}; toolbar toggle will be a no-op");
    }

    APP_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.as_mut().unwrap();
        // Boot has exactly one window; the rest are opened later and
        // get their own resumed/resized through the normal path.
        let ctx = &state.windows[0];
        state.app.resumed(ctx);
        // Deliver an initial Resized so the app sizes its renderer.
        let (w, h) = ctx.inner_size_phys();
        state.app.resized(ctx, w, h);
        if ctx.redraw_pending.replace(false) {
            state.app.redraw(ctx);
        }
    });
    // Drain anything `resumed` / `redraw` queued via the dev_window
    // deferral path (e.g. `apply_saved_frame` from L1).  Same reason
    // `dispatch_event` drains after its borrow drops — AppKit calls
    // here fire `windowDidMove:` etc. into our delegate, which
    // dispatch_event_pub's into APP_STATE → if APP_STATE were still
    // borrowed we'd re-enter and panic.
    crate::dev_window::drain_pending_actions();

    // 6. Run.  Returns after dispatch_event sees an exit request.
    { nsapp.run() };

    // Drop app state on the main thread so `Drop`s for sessions /
    // renderers fire here.
    APP_STATE.with(|cell| *cell.borrow_mut() = None);
}

// ---------------------------------------------------------------------------
// NSEvent → MarspotKeyEvent
// ---------------------------------------------------------------------------

fn nsevent_modifiers(event: &NSEvent) -> Modifiers {
    let flags = { event.modifierFlags() };
    Modifiers {
        shift: flags.contains(NSEventModifierFlags::Shift),
        control: flags.contains(NSEventModifierFlags::Control),
        alt: flags.contains(NSEventModifierFlags::Option),
        super_: flags.contains(NSEventModifierFlags::Command),
    }
}

fn nsevent_to_mars_key(event: &NSEvent, state: KeyState) -> Option<MarspotKeyEvent> {
    let key_code = { event.keyCode() };
    let logical = match key_code {
        // Carbon HIToolbox keyCodes — stable across macOS versions.
        // Reference: <HIToolbox/Events.h> kVK_* constants.
        0x24 => LogicalKey::Named(NamedKey::Enter),
        0x4C => LogicalKey::Named(NamedKey::Enter), // numeric-keypad Enter
        0x30 => LogicalKey::Named(NamedKey::Tab),
        0x33 => LogicalKey::Named(NamedKey::Backspace),
        0x35 => LogicalKey::Named(NamedKey::Escape),
        0x7E => LogicalKey::Named(NamedKey::ArrowUp),
        0x7D => LogicalKey::Named(NamedKey::ArrowDown),
        0x7B => LogicalKey::Named(NamedKey::ArrowLeft),
        0x7C => LogicalKey::Named(NamedKey::ArrowRight),
        // Navigation cluster — VT220 / xterm contract.
        0x74 => LogicalKey::Named(NamedKey::PageUp),
        0x79 => LogicalKey::Named(NamedKey::PageDown),
        0x73 => LogicalKey::Named(NamedKey::Home),
        0x77 => LogicalKey::Named(NamedKey::End),
        // Mac keyboards don't have a separate Insert; Help (kVK_Help =
        // 0x72) sits in the Insert position on classic ANSI layouts
        // and macOS apps that need Insert read it from there.
        0x72 => LogicalKey::Named(NamedKey::Insert),
        // ForwardDelete (kVK_ForwardDelete) — fn+Delete on laptops.
        // The regular `Delete` key (top-right of letter cluster) is
        // Backspace above (kVK_Delete = 0x33).
        0x75 => LogicalKey::Named(NamedKey::Delete),
        // F1-F12 — Apple's row is permuted, not sequential.
        0x7A => LogicalKey::Named(NamedKey::F1),
        0x78 => LogicalKey::Named(NamedKey::F2),
        0x63 => LogicalKey::Named(NamedKey::F3),
        0x76 => LogicalKey::Named(NamedKey::F4),
        0x60 => LogicalKey::Named(NamedKey::F5),
        0x61 => LogicalKey::Named(NamedKey::F6),
        0x62 => LogicalKey::Named(NamedKey::F7),
        0x64 => LogicalKey::Named(NamedKey::F8),
        0x65 => LogicalKey::Named(NamedKey::F9),
        0x6D => LogicalKey::Named(NamedKey::F10),
        0x67 => LogicalKey::Named(NamedKey::F11),
        0x6F => LogicalKey::Named(NamedKey::F12),
        _ => {
            // Modifier-independent character.  charactersIgnoringModifiers
            // gives "a" for both `a` and `shift+a`.
            let s_opt = { event.charactersIgnoringModifiers() };
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
        { event.characters() }.map(|s| s.to_string())
    };

    Some(MarspotKeyEvent {
        state,
        logical,
        text,
    })
}


#[cfg(test)]
mod window_op_tests {
    use super::*;

    fn attrs() -> WindowAttrs {
        WindowAttrs {
            title: "t".into(),
            width_logical: 100.0,
            height_logical: 100.0,
            bg: (0.0, 0.0, 0.0),
            frame_pt: None,
        }
    }

    /// Every event handler runs inside the `APP_STATE` borrow, so a
    /// window API that touches `APP_STATE` panics the instant a handler
    /// calls it — which is exactly what Cmd-N did the first time it
    /// shipped: `RefCell already borrowed`, app gone, sessions orphaned.
    ///
    /// The remedy is that these three queue or read a mirror instead.
    /// This holds the borrow the way a dispatch does and requires them
    /// to stay quiet.
    #[test]
    fn window_api_is_callable_from_inside_a_dispatch() {
        APP_STATE.with(|cell| {
            let _dispatch_borrow = cell.borrow_mut();

            open_window(7, &attrs());
            close_window(7);
            let _ = window_count();
        });

        // Both ops must have been queued rather than applied.
        let queued = PENDING_WINDOW_OPS.with(|q| q.borrow().len());
        assert_eq!(queued, 2, "window ops must be deferred, not applied");
        PENDING_WINDOW_OPS.with(|q| q.borrow_mut().clear());
    }
}

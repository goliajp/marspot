//! Applying the window opens, closes and raises queued while an event
//! handler held the app's state, and asking AppKit to quit.

use super::*;

/// Apply queued window opens / closes.
///
/// Runs after the `APP_STATE` borrow is released, so the AppKit calls
/// here are free to fire delegate notifications back through
/// `dispatch_event_for`.
pub(super) fn drain_pending_windows() {
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
                    last_windowed_lights_right: Cell::new(0.0),
                    exiting_full_screen: Cell::new(false),
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
                        dock::activate_over_others(&NSApplication::sharedApplication(mtm));
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
            WindowOp::Raise(window_id) => {
                let win = APP_STATE.with(|cell| {
                    let slot = cell.borrow();
                    let state = slot.as_ref()?;
                    let i = state.window_index(window_id)?;
                    Some(state.windows[i].nswindow.clone())
                });
                if let Some(w) = win {
                    if w.isMiniaturized() {
                        w.deminiaturize(None);
                    }
                    w.makeKeyAndOrderFront(None);
                }
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

/// Ask AppKit to quit, exactly as the dock's Quit item and Cmd-Q do.
///
/// A test cannot click a dock menu, and the thing worth testing is the
/// whole route — `terminate:` → `applicationShouldTerminate:` → the
/// app's `quit_requested` — not just the handler at the end of it.
pub fn request_terminate() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    // Next run-loop turn, not now.  `terminate:` calls
    // `applicationShouldTerminate:` synchronously, which dispatches an
    // event, which borrows the app state — and a caller inside an
    // event callback is already holding that borrow.  Calling it
    // straight took the app down with `RefCell already borrowed`
    // (2026-09-28).  A real quit arrives from the event loop with
    // nothing borrowed, and this makes every caller look the same.
    unsafe {
        let _: () = objc2::msg_send![
            &*app,
            performSelector: objc2::sel!(terminate:),
            withObject: std::ptr::null::<objc2::runtime::AnyObject>(),
            afterDelay: 0.0f64,
        ];
    }
}

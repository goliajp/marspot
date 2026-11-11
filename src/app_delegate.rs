//! The application's delegate: where a request to quit arrives.
//!
//! Cmd-Q, the Quit menu item, the Dock's Quit, logout and
//! `NSRunningApplication.terminate` all end in `terminate:`, which asks
//! `applicationShouldTerminate:`.  The answer is always "cancel": the
//! app's own `quit_requested` does the shutdown and stops the run loop,
//! where AppKit's would go straight to `exit(0)` past all of it.
//!
//! This used to be the boot window's delegate wearing a second hat.
//! Closing that window while another stayed open dropped the window's
//! context, and every quit request after that was addressed to a window
//! that no longer existed and thrown away: `terminate` answered `true`
//! and nothing happened (2026-11-11).  The delegate is now an object of
//! its own, kept for the life of the process, and the request goes to
//! whichever window is still there.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};

use crate::app::{dispatch_event_pub, EventKind};

thread_local! {
    /// `NSApplication` holds its delegate weakly.
    static DELEGATE: RefCell<Option<Retained<AppDelegate>>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The application delegate is messaged on the main thread.
    // - No ivars and no Drop logic.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarspotAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            dispatch_event_pub(EventKind::QuitRequested);
            NSApplicationTerminateReply::TerminateCancel
        }
    }
);

pub(crate) fn install(nsapp: &NSApplication, mtm: MainThreadMarker) {
    let delegate: Retained<AppDelegate> = {
        let this = AppDelegate::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's `init`, on a freshly allocated subclass.
        unsafe { msg_send![super(this), init] }
    };
    nsapp.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|d| *d.borrow_mut() = Some(delegate));
}

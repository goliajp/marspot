//! Programs a pane started that show up as applications.
//!
//! A program run from a pane that registers with LaunchServices (an
//! emulator, a GUI tool started by its binary) gets a Dock icon that
//! says nothing about where it came from, and quitting that icon ends
//! what shares its coalition: the pane it was started in.  The system
//! says when an application appears; this finds the pane and logs it.
//! Nothing is drawn.  An application opened through `open` belongs to
//! no pane and is passed over.

use std::cell::RefCell;

use marspot::lx_event;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey, NSWorkspaceDidLaunchApplicationNotification,
};
use objc2_foundation::{NSDictionary, NSNotification};

thread_local! {
    /// The notification center holds its observers unretained.
    static WATCH: RefCell<Option<Retained<Watch>>> = const { RefCell::new(None) };
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The workspace's notifications arrive on the main thread.
    // - No ivars and no Drop logic.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarspotPaneAppWatch"]
    struct Watch;

    impl Watch {
        #[unsafe(method(launched:))]
        fn launched(&self, note: &NSNotification) {
            let Some(info) = note.userInfo() else { return };
            let info: &NSDictionary = &info;
            // SAFETY: the workspace documents this key's value as the
            // NSRunningApplication that launched.
            let app: Option<Retained<NSRunningApplication>> =
                unsafe { msg_send![info, objectForKey: NSWorkspaceApplicationKey] };
            let Some(app) = app else { return };
            let pid = app.processIdentifier();
            let Some(sid) = pane_of(pid) else { return };
            let name = app.localizedName().map(|n| n.to_string()).unwrap_or_default();
            lx_event!(
                "PANE_APP",
                "a program started in a pane registered as an application",
                sid = sid,
                pid = pid,
                name = name
            );
        }
    }
);

/// Start listening for applications that appear.
pub(super) fn watch() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let watch: Retained<Watch> = {
        let this = Watch::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's `init`, on a freshly allocated subclass.
        unsafe { msg_send![super(this), init] }
    };
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: the observer outlives the registration (it is kept for
    // the life of the thread), and its selector takes the notification.
    unsafe {
        center.addObserver_selector_name_object(
            &watch,
            sel!(launched:),
            Some(NSWorkspaceDidLaunchApplicationNotification),
            None,
        );
    }
    WATCH.with(|w| *w.borrow_mut() = Some(watch));
}

/// The session whose pane started `pid`: the pane whose shell is among
/// its parents, or, for a program that left its parents, the one pane
/// whose shell shares its coalition.
fn pane_of(pid: i32) -> Option<u64> {
    let obs = crate::observe::shared().latest();
    let shells = || obs.sessions.iter().filter(|s| s.live && s.entry.shell_child_pid > 1);
    let mut at = pid;
    for _ in 0..64 {
        if let Some(s) = shells().find(|s| s.entry.shell_child_pid == at) {
            return Some(s.entry.id);
        }
        at = marspot::pidtree::proc_row(at)?.ppid;
        if at <= 1 {
            break;
        }
    }
    let coalition = coalition_of(pid);
    if coalition == 0 || coalition == coalition_of(std::process::id() as i32) {
        return None;
    }
    let mut sharing = shells().filter(|s| coalition_of(s.entry.shell_child_pid) == coalition);
    let one = sharing.next()?;
    sharing.next().is_none().then_some(one.entry.id)
}

/// The jetsam coalition `pid` is in, 0 when the system will not say.
fn coalition_of(pid: i32) -> u64 {
    // `PROC_PIDCOALITIONINFO` and its struct, from the system's
    // private half of <sys/proc_info.h>
    const FLAVOR: libc::c_int = 20;
    #[repr(C)]
    #[derive(Default)]
    struct Info {
        id: [u64; 2],
        reserved: [u64; 3],
    }
    let mut info = Info::default();
    let size = std::mem::size_of::<Info>() as libc::c_int;
    // SAFETY: the buffer is `size` bytes; an unknown flavor fails
    // without writing.
    let n = unsafe { libc::proc_pidinfo(pid, FLAVOR, 0, (&raw mut info).cast(), size) };
    if n == size { info.id[1] } else { 0 }
}

//! Dev seam: lose this image's registration the way an exec can.
//!
//! The race that loses it is rare on a quiet machine, so a test that
//! wants every exec to lose it arms the seam: in a sandbox
//! (`MARSPOT_DEV_SANDBOX`), with the file `dev-ae-drop` in the state
//! dir, an image that came from an exec, not from a replacement for a
//! lost registration, opens a connection of its own to appleeventsd and
//! drops it.  appleeventsd then removes every app with this pid, as it
//! does for the old image's connection.  The file says when: `first`,
//! before the first look, or `last`, before the last one (by then a
//! quit may have made the app a resident).  A file and not a variable:
//! a resident that becomes the app again does not pass the dev
//! variables on.

use std::ffi::{c_char, c_void};
use std::sync::atomic::Ordering;
use std::time::Duration;

const SERVICE: &std::ffi::CStr = c"com.apple.coreservices.appleevents";

/// How long the connection is kept before it is dropped: long enough
/// for appleeventsd to have accepted it.
const HELD: Duration = Duration::from_millis(50);

/// Most rounds of [`HELD`] to wait for the removal.
const WAIT_MOST: u32 = 40;

/// The longest the seam can take: the connection held, then every
/// round's pause and child.
const SEAM_MOST_MS: u64 =
    (HELD.as_millis() + WAIT_MOST as u128 * (HELD.as_millis() + super::child::CHILD_MOST.as_millis())) as u64;

pub(super) fn drop_registration_if_armed(retries: u32, at: &str) {
    if retries > 0 || std::env::var_os("MARSPOT_DEV_SANDBOX").is_none() {
        return;
    }
    let armed = std::fs::read_to_string(marspot_term::paths::state_root().join("dev-ae-drop"));
    if !armed.is_ok_and(|w| w.trim() == at) {
        return;
    }
    // the frames' deadline waits for the seam: by its longest at first,
    // by what it took once done
    let started = std::time::Instant::now();
    super::SEAM_MS.fetch_add(SEAM_MOST_MS, Ordering::Relaxed);
    let handler = block2::RcBlock::new(|_event: *mut c_void| {});
    // SAFETY: libxpc calls on a connection and a message made here;
    // setting the handler copies the block, so dropping ours whenever
    // is fine.
    unsafe {
        let conn = xpc_connection_create_mach_service(SERVICE.as_ptr(), std::ptr::null_mut(), 0);
        if conn.is_null() {
            return marspot::lx_warn!("shell.dev.ae_drop_failed", "no connection to appleeventsd");
        }
        xpc_connection_set_event_handler(conn, block2::RcBlock::as_ptr(&handler).cast());
        xpc_connection_resume(conn);
        let msg = xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0);
        xpc_connection_send_message(conn, msg);
        xpc_release(msg);
        std::thread::sleep(HELD);
        xpc_connection_cancel(conn);
        xpc_release(conn);
    }
    // appleeventsd removes the app some time after the break, later on
    // a busy machine: the look that follows has to find it removed.
    // each round's child is waited for at most `CHILD_MOST`
    let mut waited = 0;
    while waited < WAIT_MOST && super::child::registered_now() {
        std::thread::sleep(HELD);
        waited += 1;
    }
    let took = started.elapsed().as_millis() as u64;
    super::SEAM_MS.fetch_sub(SEAM_MOST_MS - took.min(SEAM_MOST_MS), Ordering::Relaxed);
    marspot::lx_event!(
        "DEV_AE_DROP",
        "dropped a connection to appleeventsd to lose this image's registration",
        at = at,
        waited_ms = waited * HELD.as_millis() as u32
    );
}

unsafe extern "C" {
    fn xpc_connection_create_mach_service(
        name: *const c_char,
        queue: *mut c_void,
        flags: u64,
    ) -> *mut c_void;
    fn xpc_connection_set_event_handler(conn: *mut c_void, handler: *mut c_void);
    fn xpc_connection_resume(conn: *mut c_void);
    fn xpc_connection_send_message(conn: *mut c_void, msg: *mut c_void);
    fn xpc_connection_cancel(conn: *mut c_void);
    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const *mut c_void,
        count: usize,
    ) -> *mut c_void;
    fn xpc_release(obj: *mut c_void);
}

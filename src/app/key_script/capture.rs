//! Watching the app's own window from outside its drawing: what the
//! window server composites, captured until it changes.

use std::ffi::c_void;
use std::sync::mpsc::Receiver;

use crate::latency_trace::{self as trace, Stage};

#[repr(C)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

type CreateImage = unsafe extern "C" fn(Rect, u32, u32, u32) -> *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetDataProvider(image: *mut c_void) -> *mut c_void;
    fn CGDataProviderCopyData(provider: *mut c_void) -> *const c_void;
    fn CGImageRelease(image: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFRelease(cf: *const c_void);
}

/// Just this window, without its shadow, one pixel per point.
const ONLY_THIS_WINDOW: u32 = 1 << 3;
const NO_FRAMING_NOMINAL: u32 = 1 | 1 << 4;
/// How long after a key to keep looking before giving it up.
const GIVE_UP_NS: u64 = 200_000_000;

/// A hash of the window's pixels as composited, or `None` when the
/// capture gave no image.
fn capture(create: CreateImage, window: u32) -> Option<u64> {
    // the null rectangle: the bounds of the windows asked for
    let all = Rect { x: f64::INFINITY, y: f64::INFINITY, w: 0.0, h: 0.0 };
    // SAFETY: the function was looked up by its own name and is called
    // with its own signature; every object it and the two calls after
    // it return is released below, once, after its bytes are read
    unsafe {
        let image = create(all, ONLY_THIS_WINDOW, window, NO_FRAMING_NOMINAL);
        if image.is_null() {
            return None;
        }
        let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        if !data.is_null() {
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
            for word in bytes.chunks_exact(8) {
                h = (h ^ u64::from_le_bytes(word.try_into().unwrap())).wrapping_mul(0x0000_0100_0000_01b3);
            }
            CFRelease(data);
        }
        CGImageRelease(image);
        Some(h)
    }
}

/// For every `(window, time the key was sent)` received: capture the
/// window until it differs from what it showed before the key, and
/// mark when the capture that saw the change returned.
pub(super) fn watch(keys: Receiver<(u32, u64)>) {
    // SAFETY: a lookup by name in the images already loaded
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CGWindowListCreateImage".as_ptr()) };
    if symbol.is_null() {
        crate::lx_event!("KEY_SCRIPT_NO_CAPTURE", "CGWindowListCreateImage is not in this system");
        return;
    }
    // SAFETY: the symbol is that function, whose signature this is
    let create: CreateImage = unsafe { std::mem::transmute(symbol) };
    let mut before = None;
    for (window, sent_ns) in keys {
        if before.is_none() {
            before = capture(create, window);
        }
        loop {
            let t0 = trace::now_ns();
            let now = capture(create, window);
            let t1 = trace::now_ns();
            if now.is_some() && now != before {
                trace::mark_at(Stage::OnScreen, ((t1 - t0) / 1000) as u32, t1);
                before = now;
                break;
            }
            if t1.saturating_sub(sent_ns) > GIVE_UP_NS {
                break;
            }
        }
    }
}

//! Key presses played into a sandboxed app, for timing them.
//!
//! Nothing outside the app can press a key on a machine nobody sits
//! at, so the app sends itself the events: each is a real `NSEvent`
//! handed to the front window, which routes it to the view's
//! `keyDown:` as it would one from the keyboard.  What is not
//! exercised is the system's own delivery of the event to the app.
//!
//! `MARSPOT_KEY_SCRIPT=<count>,<interval_ms>,<start_ms>[,capture]` and
//! only with `MARSPOT_DEV_SANDBOX` set, in a build with the
//! `latency-trace` feature.  With `capture`, a second thread captures
//! the app's own window over and over after each key until what the
//! window server shows has changed, and marks when: a check on the
//! other marks that does not go through this app's drawing at all.
//! It loads the window server while a frame is on its way, so the
//! per-stage times of such a run are not the ones to quote.  The keys are runs of twenty `x` then twenty backspaces,
//! so the line never wraps; the gap between two keys is the interval
//! plus up to a quarter of it, so presses do not fall in step with the
//! display.

#[cfg(not(feature = "latency-trace"))]
#[inline(always)]
pub(crate) fn trace_key_down(_event: &objc2_app_kit::NSEvent) {}

#[cfg(not(feature = "latency-trace"))]
#[inline(always)]
pub(crate) fn start(_mtm: objc2::MainThreadMarker) {}

#[cfg(feature = "latency-trace")]
pub(crate) use on::{start, trace_key_down};

#[cfg(feature = "latency-trace")]
mod capture;

#[cfg(feature = "latency-trace")]
mod on {
    use std::cell::Cell;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
    use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::{NSPoint, NSProcessInfo, NSString};

    use crate::latency_trace::{self as trace, Stage};

    const RUN: u32 = 20;

    thread_local! {
        static WATCHER: Cell<Option<std::sync::mpsc::Sender<(u32, u64)>>> = const { Cell::new(None) };
        static LEFT: Cell<u32> = const { Cell::new(0) };
        static SENT: Cell<u32> = const { Cell::new(0) };
        static INTERVAL_MS: Cell<u32> = const { Cell::new(0) };
        static RNG: Cell<u32> = const { Cell::new(0x2545_f491) };
    }

    pub(crate) fn trace_key_down(event: &NSEvent) {
        if trace::enabled() {
            trace::mark_at(Stage::KeyEventTime, 0, (event.timestamp() * 1e9) as u64);
            trace::mark(Stage::KeyDown, 0);
        }
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and this
        // class has no instance state and no Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "MarspotKeyScript"]
        struct KeyScript;

        unsafe impl NSObjectProtocol for KeyScript {}

        impl KeyScript {
            #[unsafe(method(playKeyScript:))]
            fn play(&self, _arg: Option<&AnyObject>) {
                step(self, self.mtm());
            }
        }
    );

    fn schedule(target: &AnyObject, delay_s: f64) {
        // SAFETY: the target is never released and answers
        // `playKeyScript:`, which takes the nil it is sent.
        unsafe {
            let _: () = msg_send![
                target,
                performSelector: sel!(playKeyScript:),
                withObject: std::ptr::null::<AnyObject>(),
                afterDelay: delay_s,
            ];
        }
    }

    pub(crate) fn start(mtm: MainThreadMarker) {
        if std::env::var_os("MARSPOT_DEV_SANDBOX").is_none() {
            return;
        }
        let Ok(text) = std::env::var("MARSPOT_KEY_SCRIPT") else { return };
        let capture = text.ends_with(",capture");
        let mut parts = text.trim_end_matches(",capture").split(',').map(|p| p.trim().parse::<u32>());
        let (Some(Ok(count)), Some(Ok(interval)), Some(Ok(start))) =
            (parts.next(), parts.next(), parts.next())
        else {
            crate::lx_event!("KEY_SCRIPT_BAD", "MARSPOT_KEY_SCRIPT is not count,interval_ms,start_ms");
            return;
        };
        LEFT.set(count);
        INTERVAL_MS.set(interval);
        if capture {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || super::capture::watch(rx));
            WATCHER.set(Some(tx));
        }
        let target: Retained<KeyScript> = {
            let this = KeyScript::alloc(mtm).set_ivars(());
            // SAFETY: NSObject's `init`, on a freshly allocated subclass.
            unsafe { msg_send![super(this), init] }
        };
        schedule(&target, start as f64 / 1000.0);
        // scheduled calls hold it only while one is pending
        std::mem::forget(target);
    }

    fn step(target: &KeyScript, mtm: MainThreadMarker) {
        let left = LEFT.get();
        if left == 0 {
            trace::flush();
            crate::lx_event!("KEY_SCRIPT_DONE", "every scripted key was sent", keys = SENT.get());
            return;
        }
        LEFT.set(left - 1);
        let n = SENT.get();
        SENT.set(n + 1);
        let erase = (n / RUN) % 2 == 1;
        let (chars, code) = if erase { ("\u{7f}", 51u16) } else { ("x", 7u16) };
        let nsapp = NSApplication::sharedApplication(mtm);
        if let Some(window) = nsapp.orderedWindows().firstObject() {
            let chars = NSString::from_str(chars);
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags::empty(),
                NSProcessInfo::processInfo().systemUptime(),
                window.windowNumber(),
                None,
                &chars,
                &chars,
                false,
                code,
            );
            if let Some(event) = event {
                let watcher = WATCHER.take();
                if let Some(tx) = &watcher {
                    let _ = tx.send((window.windowNumber() as u32, trace::now_ns()));
                }
                WATCHER.set(watcher);
                window.sendEvent(&event);
            }
        }
        let r = RNG.get().wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        RNG.set(r);
        let interval = INTERVAL_MS.get();
        let jitter = (r >> 8) % (interval / 4).max(1);
        schedule(target, (interval + jitter) as f64 / 1000.0);
    }
}

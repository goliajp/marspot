//! Keyboard input — the AppKit-coupled half.
//!
//! The portable key types and the `key_event_to_bytes` mapping moved
//! to `marspot_term::input_core` (no window-system dependency) so the
//! light per-session process can encode keystrokes.  This module
//! re-exports them and adds the only GUI-coupled piece: the macOS
//! clipboard accessors, which `key_event_to_bytes` takes by injection.

pub use marspot_term::input_core::*;

use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Names a pasteboard for copy and paste to go through instead of the
/// general one.  A sandbox sets it, so a test run neither reads nor
/// replaces what the person at the machine has copied.
pub const PASTEBOARD_ENV: &str = "MARSPOT_PASTEBOARD";

/// The pasteboard copy and paste use: the general one unless
/// [`PASTEBOARD_ENV`] names another.
pub fn pasteboard() -> Retained<NSPasteboard> {
    match std::env::var(PASTEBOARD_ENV) {
        Ok(name) if !name.is_empty() => {
            NSPasteboard::pasteboardWithName(&NSString::from_str(&name))
        }
        _ => NSPasteboard::generalPasteboard(),
    }
}

/// Read the pasteboard's string, if any.  Used to
/// implement Cmd-V → write to PTY.  Returns None if the clipboard
/// holds non-text content (image, file URLs, etc.).
pub fn read_clipboard_text() -> Option<String> {
    unsafe {
        let pb = pasteboard();
        let s = pb.stringForType(NSPasteboardTypeString)?;
        Some(s.to_string())
    }
}

/// Replace the pasteboard's contents with a single
/// plain-text string.  Used to implement Cmd-C → copy current
/// terminal selection.  Returns true on success.
pub fn write_clipboard_text(text: &str) -> bool {
    unsafe {
        let pb = pasteboard();
        // clearContents must precede setString or AppKit retains
        // any prior reps and the new write may be ignored.
        pb.clearContents();
        let s = NSString::from_str(text);
        pb.setString_forType(&s, NSPasteboardTypeString)
    }
}

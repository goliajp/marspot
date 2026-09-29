//! Keyboard input — the AppKit-coupled half.
//!
//! The portable key types and the `key_event_to_bytes` mapping moved
//! to `marspot_term::input_core` (no window-system dependency) so the
//! light per-session process can encode keystrokes.  This module
//! re-exports them and adds the only GUI-coupled piece: the macOS
//! clipboard accessors, which `key_event_to_bytes` takes by injection.

pub use marspot_term::input_core::*;

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

/// Read the current macOS general-pasteboard string, if any.  Used to
/// implement Cmd-V → write to PTY.  Returns None if the clipboard
/// holds non-text content (image, file URLs, etc.).
pub fn read_clipboard_text() -> Option<String> {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let s = pb.stringForType(NSPasteboardTypeString)?;
        Some(s.to_string())
    }
}

/// Replace the macOS general pasteboard contents with a single
/// plain-text string.  Used to implement Cmd-C → copy current
/// terminal selection.  Returns true on success.
pub fn write_clipboard_text(text: &str) -> bool {
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        // clearContents must precede setString or AppKit retains
        // any prior reps and the new write may be ignored.
        pb.clearContents();
        let s = objc2_foundation::NSString::from_str(text);
        pb.setString_forType(&s, NSPasteboardTypeString)
    }
}

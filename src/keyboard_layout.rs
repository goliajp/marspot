//! What key is under the finger, independent of what it types.
//!
//! The kitty keyboard protocol asks for two things AppKit does not
//! hand over.  `charactersIgnoringModifiers` sounds like it answers
//! the first and does not: Apple's own documentation says it ignores
//! every modifier "except for Shift", so `shift+1` arrives as `!`
//! where the protocol wants `1`.  And nothing in AppKit reports what
//! a physical key would be on a US layout, which is what lets a
//! program keep `ctrl+z` on the bottom-left key for someone typing on
//! AZERTY.
//!
//! Both come from Carbon.  [`unshifted_char`] runs the current layout
//! backwards through `UCKeyTranslate` with no modifiers applied;
//! [`us_layout_char`] is a table of the ANSI positions, taken from
//! `<HIToolbox/Events.h>`'s own `kVK_ANSI_*` constants.
//!
//! Neither is cached.  A key event happens at human speed — a held
//! key repeats a few dozen times a second — and the alternative is
//! tracking `kTISNotifySelectedKeyboardInputSourceChanged` to know
//! when a cache went stale, which is more machinery than the cache
//! could save.
//!
//! The Carbon call is serialised behind a lock.  Two threads calling
//! it at once aborts the process — measured, running this module's
//! own tests under `cargo test`'s default thread pool, which is
//! exactly how a future reader would first meet it.  In marspot the
//! caller is the AppKit event handler and there is only one of it, so
//! the lock is never contended; making the function safe to call from
//! anywhere costs an uncontended mutex per keystroke and removes a
//! rule that could only otherwise live in a comment.

use std::ffi::c_void;
use std::sync::Mutex;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *mut c_void;
    static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    fn LMGetKbdType() -> u8;
    #[allow(clippy::too_many_arguments)]
    fn UCKeyTranslate(
        layout: *const u8,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFRelease(cf: *const c_void);
}

/// `kUCKeyActionDisplay` — what the key shows, not a keystroke being
/// composed.  Paired with `kUCKeyTranslateNoDeadKeysMask` so a dead
/// key (`´` on a French layout) reports itself instead of swallowing
/// the next press into this lookup.
const K_UC_KEY_ACTION_DISPLAY: u16 = 3;
const K_UC_KEY_TRANSLATE_NO_DEAD_KEYS_MASK: u32 = 1;

/// What this physical key types with no modifiers at all, under the
/// layout in use right now.  `None` when the key produces nothing
/// printable (function keys, arrows) or the layout cannot be read.
///
/// This is the codepoint the kitty protocol calls the key itself:
/// `shift+1` is the `1` key, and `1` is what a program binding to it
/// has to see.
pub fn unshifted_char(key_code: u16) -> Option<char> {
    /// See the module docs: concurrent Carbon calls here abort.
    static CARBON: Mutex<()> = Mutex::new(());
    // A poisoned lock would mean a previous caller panicked between
    // the copy and the release — the guard is still what we need, and
    // refusing to answer would turn one panic into every keystroke
    // after it.
    let _guard = CARBON.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY: TISCopyCurrentKeyboardLayoutInputSource returns either
    // null or a +1 reference we release below.  Every pointer derived
    // from it is used only while that reference is alive, and the
    // lock above keeps a second thread out of the whole span.
    unsafe {
        let source = TISCopyCurrentKeyboardLayoutInputSource();
        if source.is_null() {
            return None;
        }
        let out = unshifted_from_source(source, key_code);
        CFRelease(source.cast());
        out
    }
}

/// SAFETY: `source` must be a live TIS input source.
unsafe fn unshifted_from_source(source: *mut c_void, key_code: u16) -> Option<char> {
    // SAFETY: the property is a borrowed CFDataRef owned by `source`;
    // its bytes stay valid for as long as the caller holds `source`.
    unsafe {
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        if data.is_null() {
            // Some input sources (a handwriting or voice source) have
            // no layout data at all.  Nothing to report.
            return None;
        }
        let layout = CFDataGetBytePtr(data.cast());
        if layout.is_null() {
            return None;
        }
        let mut dead_state: u32 = 0;
        let mut len: usize = 0;
        let mut buf = [0u16; 8];
        let status = UCKeyTranslate(
            layout,
            key_code,
            K_UC_KEY_ACTION_DISPLAY,
            0, // no modifiers: this is the whole point
            LMGetKbdType() as u32,
            K_UC_KEY_TRANSLATE_NO_DEAD_KEYS_MASK,
            &mut dead_state,
            buf.len(),
            &mut len,
            buf.as_mut_ptr(),
        );
        if status != 0 || len == 0 {
            return None;
        }
        // A single UTF-16 unit is the only shape worth reporting: the
        // protocol wants one codepoint, and a key that types a
        // multi-character string has no single one to give.
        char::decode_utf16(buf[..len].iter().copied())
            .next()
            .and_then(Result::ok)
            .filter(|c| !c.is_control())
    }
}

/// Where this physical key sits on a US ANSI keyboard.
///
/// The protocol's "base layout key": it lets a program keep a binding
/// on a key POSITION rather than on what that position types, so
/// `ctrl+z` stays bottom-row-left whether the layout is QWERTY,
/// AZERTY or Dvorak.
///
/// Values are the `kVK_ANSI_*` constants from `<HIToolbox/Events.h>`,
/// read out of the SDK header rather than recalled — the numbering is
/// famously not alphabetical, not positional, and has two gaps (0x0A
/// is ISO section, 0x24 is Return).
pub const fn us_layout_char(key_code: u16) -> Option<char> {
    Some(match key_code {
        0x00 => 'a',
        0x01 => 's',
        0x02 => 'd',
        0x03 => 'f',
        0x04 => 'h',
        0x05 => 'g',
        0x06 => 'z',
        0x07 => 'x',
        0x08 => 'c',
        0x09 => 'v',
        0x0B => 'b',
        0x0C => 'q',
        0x0D => 'w',
        0x0E => 'e',
        0x0F => 'r',
        0x10 => 'y',
        0x11 => 't',
        0x12 => '1',
        0x13 => '2',
        0x14 => '3',
        0x15 => '4',
        0x16 => '6',
        0x17 => '5',
        0x18 => '=',
        0x19 => '9',
        0x1A => '7',
        0x1B => '-',
        0x1C => '8',
        0x1D => '0',
        0x1E => ']',
        0x1F => 'o',
        0x20 => 'u',
        0x21 => '[',
        0x22 => 'i',
        0x23 => 'p',
        0x25 => 'l',
        0x26 => 'j',
        0x27 => '\'',
        0x28 => 'k',
        0x29 => ';',
        0x2A => '\\',
        0x2B => ',',
        0x2C => '/',
        0x2D => 'n',
        0x2E => 'm',
        0x2F => '.',
        0x32 => '`',
        0x31 => ' ',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_us_table_covers_every_ansi_position_exactly_once() {
        // A table written by hand is a table with a duplicate in it.
        // 47 printable ANSI positions plus space.
        let mut seen = std::collections::HashSet::new();
        let mut n = 0;
        for code in 0u16..=0x7F {
            if let Some(c) = us_layout_char(code) {
                assert!(seen.insert(c), "{c:?} appears twice (code {code:#04x})");
                n += 1;
            }
        }
        assert_eq!(n, 48, "expected 47 ANSI keys + space, got {n}");
    }

    #[test]
    fn the_lookup_survives_being_called_from_everywhere_at_once() {
        // The regression this guards: without the lock, this aborts
        // the process rather than failing a test.
        let hands: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for code in 0u16..0x33 {
                        let _ = unshifted_char(code);
                    }
                })
            })
            .collect();
        for h in hands {
            h.join().expect("no thread died");
        }
    }

    #[test]
    fn the_current_layout_agrees_with_the_us_table_on_letters() {
        // Not an assertion about the machine's layout — it is about
        // the two paths being the same KIND of answer.  On a US
        // layout they must agree; on any other, the point of having
        // both is that they don't, so only check when they can.
        let Some(a) = unshifted_char(0x00) else {
            return; // no layout data in this environment
        };
        if a == 'a' {
            for code in [0x06u16, 0x0C, 0x12, 0x2D] {
                assert_eq!(
                    unshifted_char(code),
                    us_layout_char(code),
                    "code {code:#04x}"
                );
            }
        }
    }

    #[test]
    fn a_shifted_number_key_reports_the_number() {
        // The bug this module exists for.  Only meaningful on a
        // layout where the top row is digits, which is every layout
        // this would be tested on, but check rather than assume.
        if unshifted_char(0x12) == Some('1') {
            assert_eq!(unshifted_char(0x13), Some('2'));
            assert_eq!(unshifted_char(0x1D), Some('0'));
        }
    }
}

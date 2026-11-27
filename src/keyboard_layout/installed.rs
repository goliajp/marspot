//! The layouts Apple ships, read by name: what a key types under one
//! of them, whatever layout the machine running the tests is set to.
//!
//! Asking the Text Input Sources API for any layout registers the
//! process with LaunchServices, and the suite runs every test in a
//! process of its own.  So the tests read each layout's `uchr` table
//! from a file recorded from that API, and the live checks below
//! compare the files with what the system has now.  They are ignored in
//! the suite; `bin/appkit-fixtures.sh` runs them (with `--write`,
//! rewrites the files).

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;

use super::{kTISPropertyUnicodeKeyLayoutData, layout_of, typed_from_layout, CFRelease, TISGetInputSourceProperty};

/// Carbon's Cmd, as `UCKeyTranslate` takes it.
pub const CMD: u32 = 1;
/// `LMGetKbdType` values of Apple's ANSI, ISO and JIS keyboards.
pub const ANSI: u32 = 40;
pub const ISO: u32 = 41;
pub const JIS: u32 = 42;

/// `UCKeyTranslate` reads the table's 32-bit fields in place.
#[repr(C, align(8))]
struct Aligned<T: ?Sized>(T);

macro_rules! recorded {
    ($($id:literal),* $(,)?) => {
        &[$(($id, {
            static TABLE: &Aligned<[u8]> = &Aligned(*include_bytes!(concat!("../../tests/fixtures/appkit/", $id, ".uchr")));
            &TABLE.0
        })),*]
    };
}

/// Every layout a test reads, by input source id.
static RECORDED: &[(&str, &[u8])] = recorded!(
    "com.apple.keylayout.US",
    "com.apple.keylayout.ABC",
    "com.apple.keylayout.German",
    "com.apple.keylayout.French",
    "com.apple.keylayout.DVORAK-QWERTYCMD",
    "com.apple.keylayout.Russian",
);

/// What `key_code` types under the layout `source_id` with `modifiers`
/// held.  Panics when the layout was not recorded.
pub fn typed(source_id: &str, kbd_type: u32, key_code: u16, modifiers: u32) -> Option<char> {
    let (_, table) = RECORDED
        .iter()
        .find(|(id, _)| *id == source_id)
        .unwrap_or_else(|| panic!("layout {source_id} is not recorded: add it here and run bin/appkit-fixtures.sh --write"));
    // SAFETY: a whole table, aligned, in static memory
    unsafe { typed_from_layout(table.as_ptr(), key_code, modifiers, kbd_type) }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCreateInputSourceList(properties: *const c_void, include_all_installed: u8) -> *const c_void;
    static kTISPropertyInputSourceID: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDataGetLength(data: *const c_void) -> isize;
}

/// The `uchr` table the system has now for `source_id`.
fn live_table(source_id: &str) -> Vec<u8> {
    let id = CFString::new(source_id);
    // SAFETY: the list is a +1 array released below; the source and its
    // table are borrowed from it and copied out while it is alive
    unsafe {
        let key = CFString::wrap_under_get_rule(kTISPropertyInputSourceID.cast());
        let filter = CFDictionary::from_CFType_pairs(&[(key, id)]);
        let list = TISCreateInputSourceList(filter.as_concrete_TypeRef().cast(), 1);
        assert!(!list.is_null() && CFArrayGetCount(list) > 0, "layout {source_id} is not installed");
        let source = CFArrayGetValueAtIndex(list, 0) as *mut c_void;
        let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
        assert!(!data.is_null(), "layout {source_id} has no uchr table");
        let bytes = std::slice::from_raw_parts(layout_of(source), CFDataGetLength(data) as usize).to_vec();
        CFRelease(list);
        bytes
    }
}

#[test]
#[ignore = "registers the test process with LaunchServices: run by bin/appkit-fixtures.sh"]
fn live_appkit_layouts_are_the_recorded_ones() {
    let write = std::env::var_os("MARSPOT_TEST_WRITE_APPKIT_FIXTURES").is_some();
    for (id, table) in RECORDED {
        let now = live_table(id);
        if write {
            let path = format!("{}/tests/fixtures/appkit/{id}.uchr", env!("CARGO_MANIFEST_DIR"));
            std::fs::write(path, &now).expect("write the table");
        } else {
            assert!(now == *table, "{id} is not the recorded table: run bin/appkit-fixtures.sh --write and review");
        }
    }
}

#[test]
#[ignore = "registers the test process with LaunchServices: run by bin/appkit-fixtures.sh"]
fn live_appkit_lookup_survives_being_called_from_everywhere_at_once() {
    // without the lock in `typed_live` this aborts the process rather
    // than failing a test
    let hands: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                for code in 0u16..0x33 {
                    let _ = super::typed_live(code, 0);
                }
            })
        })
        .collect();
    for h in hands {
        h.join().expect("no thread died");
    }
}

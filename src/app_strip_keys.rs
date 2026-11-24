//! The windows whose keys a pane's strip of helpers has, as the core
//! says.  There a plain `x` stops a helper, so it goes to the core as a
//! key and not to an input method, which would start composing with it.

use std::cell::RefCell;

use crate::input::Modifiers;

/// More windows than are ever open; past it the oldest is dropped.
const MAX: usize = 64;

thread_local! {
    static ON: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

pub fn set_strip_keys_for(window_id: u32, on: bool) {
    ON.with(|w| {
        let mut w = w.borrow_mut();
        w.retain(|&id| id != window_id);
        if on {
            if w.len() == MAX {
                w.remove(0);
            }
            w.push(window_id);
        }
    });
}

/// A new core: none has said anything yet.
pub fn clear_strip_keys() {
    ON.with(|w| w.borrow_mut().clear());
}

/// Whether the key with `key_code` is a plain `x` that window
/// `window_id` takes past the input method.
pub(crate) fn plain_x(window_id: u32, key_code: u16, mods: Modifiers, composing: bool) -> bool {
    let plain = !(mods.shift || mods.control || mods.alt || mods.super_ || composing);
    plain
        && crate::keyboard_layout::unshifted_char(key_code) == Some('x')
        && ON.with(|w| w.borrow().contains(&window_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_strip_only_a_plain_x_in_a_window_said_to_have_its_keys_is_taken() {
        clear_strip_keys();
        let x = (0..128).find(|&k| crate::keyboard_layout::unshifted_char(k) == Some('x')).expect("a key for x");
        let none = Modifiers::default();
        assert!(!plain_x(3, x, none, false), "nothing said");
        set_strip_keys_for(3, true);
        assert!(plain_x(3, x, none, false));
        assert!(!plain_x(4, x, none, false), "another window");
        assert!(!plain_x(3, x, none, true), "while something is composed");
        assert!(!plain_x(3, x, Modifiers { shift: true, ..none }, false));
        let other = (0..128).find(|&k| crate::keyboard_layout::unshifted_char(k) == Some('y')).unwrap();
        assert!(!plain_x(3, other, none, false));
        set_strip_keys_for(3, false);
        assert!(!plain_x(3, x, none, false));
        for id in 0..MAX as u32 + 1 {
            set_strip_keys_for(100 + id, true);
        }
        assert!(!plain_x(100, x, none, false), "the oldest went");
        assert!(plain_x(101, x, none, false));
        clear_strip_keys();
        assert!(!plain_x(101, x, none, false));
    }
}

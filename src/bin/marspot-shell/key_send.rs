//! A key on its way from AppKit to the core.
//!
//! The payload is encoded into one buffer the shell keeps and written
//! from it, so a keystroke past the first few allocates nothing here.

use marspot::input::{KeyState, MarspotKeyEvent, Modifiers};
use marspot::shell_proto::{encode_event, MsgType};

use super::core_conn::CoreConn;
use super::pump::Pump;

/// Everything `ShellApp::key_event` does with a key past its log line,
/// on the parts of the shell it needs: `buf` is the shell's key buffer,
/// `pump` and `active` what `ShellApp::send` uses.  False when the key
/// is the new-window command, which the caller runs instead.
pub(super) fn forward_key(
    pump: Option<&Pump>,
    active: Option<&CoreConn>,
    buf: &mut Vec<u8>,
    event: &MarspotKeyEvent,
    mods: Modifiers,
    window_id: u32,
) -> bool {
    if opens_window(event, mods) {
        return false;
    }
    super::pane_control::key_pressed();
    encode_key(buf, event, mods, window_id);
    super::core_spawn::send_to(pump, active, MsgType::KeyEvent, buf);
    marspot::latency_trace::mark(marspot::latency_trace::Stage::KeySent, 0);
    true
}

/// Whether this key is the one command the shell runs itself: the
/// shell owns the windows, so opening one is not the core's to do.
fn opens_window(event: &MarspotKeyEvent, mods: Modifiers) -> bool {
    event.state == KeyState::Pressed
        && marspot::commands::for_key(event, mods)
            .is_some_and(|row| row.command == marspot::commands::Command::NewWindow)
}

/// The key frame payload for `event` in `buf`, which is reused: emptied
/// first, then grown only by a key larger than any before it.
fn encode_key(
    buf: &mut Vec<u8>,
    event: &MarspotKeyEvent,
    mods: Modifiers,
    window_id: u32,
) {
    buf.clear();
    encode_event(event, mods, window_id, buf);
}

#[cfg(test)]
#[path = "key_send_tests.rs"]
mod tests;

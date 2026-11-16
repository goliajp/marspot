//! Pointer events from the window to an L3 pane's program.
//!
//! The modes this side decides from are the last frame's, and they lag
//! the session's: a report sent on them after the program has gone is
//! typed into the shell.  A session that says it checks pointer events
//! itself (`FLAG_POINTER_GATED` in its frames) is sent the event as a
//! description and encodes it from its own modes, or drops it.  An
//! older one is sent the bytes, encoded here, as it always was.

use marspot_term::shell_proto::{PointerInput, PointerKind, encode_pane_pointer_input};

use super::{L3Conn, PaneBackend};
use crate::shell_proto::{Frame, MsgType};

impl PaneBackend {
    /// Send `n` arrow-key presses to the child.
    ///
    /// For a full-screen TUI that does NOT ask for mouse reporting,
    /// this is the only way a wheel can reach it.  DECCKM decides the
    /// encoding: an app-cursor-keys program expects `ESC O A`, and
    /// sending it `ESC [ A` puts a stray `[` where it wanted a key.
    pub fn l3_inject_arrows(&mut self, up: bool, n: u32) {
        if let PaneBackend::L3(c) = self {
            let button = if up { 64 } else { 65 };
            c.send_pointer(event(PointerKind::Arrows, button, n, 0, 0));
        }
    }

    /// The pointer moved, at the cell it moved to.
    ///
    /// `button` is the one being held, or 3 for none — which is what
    /// both encodings use to mean "no button".  The motion bit (32) is
    /// what tells the program this is travel rather than a press.
    pub fn l3_inject_mouse_motion(&mut self, button: u8, col: u32, row: u32) {
        if let PaneBackend::L3(c) = self {
            c.send_pointer(event(PointerKind::Motion, button, 1, col, row));
        }
    }

    /// A button going down or coming up, at the cell the pointer is in.
    ///
    /// Reported the same way by every tracking mode a program can ask
    /// for (`?1000`, `?1002`, `?1003`).
    pub fn l3_inject_mouse_button(&mut self, button: u8, pressed: bool, col: u32, row: u32) {
        if let PaneBackend::L3(c) = self {
            let kind = if pressed {
                PointerKind::Press
            } else {
                PointerKind::Release
            };
            c.send_pointer(event(kind, button, 1, col, row));
        }
    }

    /// `n_ticks` wheel ticks (`button` 64 up, 65 down), reported at the
    /// middle of the pane: `cols` / `rows` are the caller's, the pane
    /// does not keep them.
    pub fn l3_inject_wheel(&mut self, button_64_or_65: u8, n_ticks: u32, cols: u16, rows: u16) {
        let x = (cols.max(1) / 2 + 1) as u32;
        let y = (rows.max(1) / 2 + 1) as u32;
        if let PaneBackend::L3(c) = self {
            c.send_pointer(event(PointerKind::Wheel, button_64_or_65, n_ticks, x, y));
        }
    }

    /// Keys a plugin bound to this pane's wheel.  A session that checks
    /// pointer events writes them only while a program other than the
    /// shell has the tty: they are an arrow or a page key, and at a
    /// prompt they recall and edit history.
    pub fn forward_program_keys(&mut self, keys: &[u8]) {
        if let PaneBackend::L3(c) = self {
            let mut ev = event(PointerKind::ProgramKeys, 0, 1, 0, 0);
            ev.keys = keys.to_vec();
            c.send_pointer(ev);
        }
    }

    /// Encode one mouse report the way an older session needs it.
    ///
    /// `button` is the VT button code before any offset: 0 left, 1
    /// middle, 2 right, 64 wheel up, 65 wheel down.  `col` / `row` are
    /// 1-based cell coordinates, the way both encodings want them.
    /// SGR (`?1006`) has no coordinate limit; the X10 form packs each
    /// field into one byte at `+32`, which stops working past column
    /// 223 — the encoding's own ceiling.
    pub(super) fn encode_mouse(
        buf: &mut Vec<u8>,
        sgr: bool,
        button: u8,
        pressed: bool,
        col: u32,
        row: u32,
    ) {
        if sgr {
            let final_byte = if pressed { 'M' } else { 'm' };
            buf.extend_from_slice(format!("\x1b[<{button};{col};{row}{final_byte}").as_bytes());
        } else {
            // X10 has no release code per button: every release is 3.
            let b = if pressed { button } else { 3 };
            buf.extend_from_slice(b"\x1b[M");
            buf.push(b + 32);
            buf.push((col.min(223) as u8) + 32);
            buf.push((row.min(223) as u8) + 32);
        }
    }
}

fn event(kind: PointerKind, button: u8, count: u32, col: u32, row: u32) -> PointerInput {
    PointerInput {
        kind,
        button,
        count: count.min(u16::MAX as u32) as u16,
        col,
        row,
        keys: Vec::new(),
    }
}

/// The modes an older session's bytes are encoded against: the last
/// frame's.
#[derive(Clone, Copy, Debug)]
pub(super) struct FrameModes {
    pub sgr: bool,
    pub app_cursor: bool,
}

/// The frame that carries `ev` to session `sid`.
///
/// `gated` is the session's own statement that it checks the event;
/// the description goes only to one that made it.
pub(super) fn pointer_frame(sid: u64, gated: bool, ev: &PointerInput, m: FrameModes) -> Frame {
    if gated {
        return Frame::new(
            MsgType::PanePointerInput,
            encode_pane_pointer_input(sid, ev),
        );
    }
    let n = ev.count.max(1) as usize;
    let mut buf = Vec::with_capacity(16 * n);
    match ev.kind {
        PointerKind::Press => {
            PaneBackend::encode_mouse(&mut buf, m.sgr, ev.button, true, ev.col, ev.row)
        }
        PointerKind::Release => {
            PaneBackend::encode_mouse(&mut buf, m.sgr, ev.button, false, ev.col, ev.row)
        }
        PointerKind::Motion => {
            PaneBackend::encode_mouse(&mut buf, m.sgr, ev.button + 32, true, ev.col, ev.row)
        }
        // a tick is a press with no release
        PointerKind::Wheel => (0..n).for_each(|_| {
            PaneBackend::encode_mouse(&mut buf, m.sgr, ev.button, true, ev.col, ev.row)
        }),
        PointerKind::Arrows => {
            let seq: &[u8] = match (m.app_cursor, ev.button == 64) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1bOB",
                (false, true) => b"\x1b[A",
                (false, false) => b"\x1b[B",
            };
            (0..n).for_each(|_| buf.extend_from_slice(seq));
        }
        PointerKind::ProgramKeys => buf.extend_from_slice(&ev.keys),
    }
    Frame::new(
        MsgType::InjectInput,
        crate::shell_proto::encode_inject_input(sid, &buf),
    )
}

impl L3Conn {
    fn send_pointer(&mut self, ev: PointerInput) {
        let modes = FrameModes {
            sgr: self.mouse_sgr_active,
            app_cursor: self.app_cursor_keys,
        };
        let frame = pointer_frame(self.session_id, self.pointer_gated, &ev, modes);
        let _ = self.control.send(frame);
    }
}

#[cfg(test)]
#[path = "pointer_tests.rs"]
mod tests;

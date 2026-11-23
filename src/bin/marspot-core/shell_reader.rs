//! The core's end of the shell's control socket: each frame L1 sends
//! becomes a `CoreEvent` for the main loop.

use super::*;
use marspot::shell_proto::{
    decode_file_drop, decode_focus, decode_hello, decode_key_event, decode_mouse, decode_ping,
    decode_preedit, decode_resize, decode_scroll, decode_surface_attach,
    decode_surface_attach_window, decode_window_chrome, decode_window_closed,
    decode_window_focus, mods_to_struct, wire_to_event,
};

fn decode_frame(f: &Frame) -> Option<CoreEvent> {
    match f.msg_type {
        MsgType::KeyEvent => decode_key_event(&f.payload).ok().map(|(w, win)| {
            trace::mark(Stage::CoreKeyRead, 0);
            let (e, m) = wire_to_event(w);
            CoreEvent::Key(e, m, win)
        }),
        MsgType::MouseDown => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, m, win)| CoreEvent::MouseDown(x, y, mods_to_struct(m), win)),
        MsgType::MouseRightDown => decode_mouse(&f.payload).ok().map(|(x, y, m, win)| {
            CoreEvent::MouseRightDown(x, y, mods_to_struct(m), win)
        }),
        MsgType::MouseDrag => marspot::shell_proto::decode_mouse_drag(&f.payload)
            .ok()
            .map(|(x, y, _, win, hw, hx, hy)| CoreEvent::MouseDrag(x, y, win, hw, hx, hy)),
        MsgType::MouseUp => marspot::shell_proto::decode_mouse_up(&f.payload)
            .ok()
            .map(|(_, _, _, win, drop, dx, dy)| CoreEvent::MouseUp(win, drop, dx, dy)),
        MsgType::MouseMove => decode_mouse(&f.payload)
            .ok()
            .map(|(x, y, _, win)| CoreEvent::MouseMove(x, y, win)),
        MsgType::Scroll => decode_scroll(&f.payload)
            .ok()
            .map(|(_dx, dy, p, win)| CoreEvent::Scroll(dy, p, win)),
        MsgType::FileDrop => decode_file_drop(&f.payload)
            .ok()
            .map(|(x, y, paths, win)| CoreEvent::FileDrop(x, y, paths, win)),
        MsgType::SurfaceAttachWindow => decode_surface_attach_window(&f.payload)
            .ok()
            .map(|(fr, bk, w, h, sc, win, slot, lights)| {
                CoreEvent::SurfaceAttachWindow(fr, bk, w, h, sc, win, slot, lights)
            }),
        MsgType::WindowChrome => decode_window_chrome(&f.payload)
            .ok()
            .map(|(win, lights)| CoreEvent::WindowChrome(win, lights)),
        MsgType::WindowClosed => decode_window_closed(&f.payload)
            .ok()
            .map(CoreEvent::WindowClosed),
        MsgType::WindowFocus => decode_window_focus(&f.payload)
            .ok()
            .map(CoreEvent::WindowFocus),
        MsgType::Focus => decode_focus(&f.payload).ok().map(CoreEvent::Focus),
        MsgType::Resize => decode_resize(&f.payload)
            .ok()
            .map(|(id, w, h, s)| CoreEvent::Resize(id, w, h, s)),
        MsgType::SurfaceAttach => decode_surface_attach(&f.payload)
            .ok()
            .map(|(f_id, b_id, w, h, s)| CoreEvent::SurfaceAttach(f_id, b_id, w, h, s)),
        MsgType::PaneRecede => marspot::shell_proto::decode_pane_recede(&f.payload)
            .ok()
            .map(|(sid, level)| CoreEvent::PaneRecede(sid, level)),
        MsgType::PaneBadge => marspot::shell_proto::decode_pane_badge(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneBadge(sid, text)),
        MsgType::PaneWheelKeys => marspot::shell_proto::decode_pane_wheel_keys(&f.payload)
            .ok()
            .map(|(sid, enter, up, down, marker)| {
                CoreEvent::PaneWheelKeys(sid, enter, up, down, marker)
            }),
        MsgType::PaneBadgeMenu => marspot::shell_proto::decode_pane_badge_menu(&f.payload)
            .ok()
            .map(|(sid, x, y, items)| CoreEvent::PaneBadgeMenu(sid, x, y, items)),
        MsgType::PaneTitle => marspot::shell_proto::decode_pane_title(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneTitle(sid, text)),
        MsgType::PaneSessionBegin => marspot::shell_proto::decode_pane_session_begin(&f.payload)
            .ok()
            .map(|(sid, caps)| CoreEvent::PaneSessionBegin(sid, caps)),
        MsgType::PaneSessionEnd => marspot::shell_proto::decode_pane_session_end(&f.payload)
            .ok()
            .map(CoreEvent::PaneSessionEnd),
        MsgType::PaneInjectPaste => marspot::shell_proto::decode_pane_inject_paste(&f.payload)
            .ok()
            .map(|(sid, text)| CoreEvent::PaneInjectPaste(sid, text)),
        MsgType::PaneHoldGrid => marspot::shell_proto::decode_pane_hold_grid(&f.payload)
            .ok()
            .map(|(sid, on)| CoreEvent::PaneHoldGrid(sid, on)),
        MsgType::PaneAgentTui => marspot::shell_proto::decode_pane_agent_tui(&f.payload)
            .ok()
            .map(|(sid, on)| CoreEvent::PaneAgentTui(sid, on)),
        MsgType::PaneRenderMarkup => marspot::shell_proto::decode_pane_render_markup(&f.payload)
            .ok()
            .map(|(sid, on)| CoreEvent::PaneRenderMarkup(sid, on)),
        MsgType::PaneResetMouseReporting => {
            marspot::shell_proto::decode_pane_reset_mouse_reporting(&f.payload)
                .ok()
                .map(CoreEvent::PaneResetMouseReporting)
        }
        MsgType::InjectInput => marspot::shell_proto::decode_inject_input(&f.payload)
            .ok()
            .map(|(sid, bytes)| CoreEvent::InjectInput(sid, bytes)),
        MsgType::Preedit => decode_preedit(&f.payload)
            .ok()
            .map(|(text, win)| CoreEvent::Preedit(text, win)),
        MsgType::Hello => decode_hello(&f.payload).ok().map(CoreEvent::Hello),
        MsgType::FramePace => Some(CoreEvent::FramePace),
        MsgType::FrameShown => marspot::shell_proto::decode_frame_shown(&f.payload)
            .ok()
            .map(|(window, surface)| CoreEvent::FrameShown(window, surface)),
        MsgType::Ping => decode_ping(&f.payload).ok().map(CoreEvent::Ping),
        MsgType::PaneType
        | MsgType::PaneWatch
        | MsgType::PaneSpawn
        | MsgType::PaneRetire
        | MsgType::PanePlace
        | MsgType::PaneSize => Some(CoreEvent::ShellPaneControl(f.clone())),
        MsgType::IconMask => marspot::shell_proto::decode_icon_mask(&f.payload)
            .ok()
            .map(|(symbol, point_px, side, mask)| CoreEvent::IconMask(symbol, point_px, side, mask)),
        _ => None,
    }
}

/// Background reader: reads framed messages off the control socket
/// until EOF / error, dispatches each into `tx`.
pub(super) fn reader_loop(mut stream: UnixStream, tx: Sender<CoreEvent>) {
    loop {
        match Frame::read_from(&mut stream) {
            Ok(None) => {
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
            Ok(Some(frame)) => {
                if let Some(ev) = decode_frame(&frame)
                    && tx.send(ev).is_err() {
                        return;
                    }
            }
            Err(e) => {
                lx_error!("core.control.read_failed", &format!("{e}"));
                let _ = tx.send(CoreEvent::Closed);
                return;
            }
        }
    }
}

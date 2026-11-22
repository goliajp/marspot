//! The thread that reads what the core sends and hands it to the main
//! thread.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;

use marspot::app::EventProxy;
use marspot::latency_trace::{self as trace, Stage};
use marspot::lx_error;
use marspot::shell_proto::{
    decode_caret_rect, decode_hello_ack, decode_pong, decode_surface_ready, Frame, MsgType,
};

use crate::ShellInbox;

pub(crate) mod pane_wakes;

pub(crate) fn control_reader_loop(mut stream: UnixStream, tx: Sender<ShellInbox>, proxy: EventProxy) {
    loop {
        match Frame::read_from(&mut stream) {
            // the core is gone: a pass finds that out now
            Ok(None) => return proxy.wake(),
            Ok(Some(frame)) => {
                let msg = match frame.msg_type {
                    MsgType::SurfaceReady => {
                        trace::mark(Stage::ShellSurfaceRead, 0);
                        decode_surface_ready(&frame.payload).ok().map(ShellInbox::SurfaceReady)
                    }
                    MsgType::HelloAck => {
                        decode_hello_ack(&frame.payload).ok().map(ShellInbox::HelloAck)
                    }
                    MsgType::FramePaceAck => Some(ShellInbox::FramePaceAck),
                    MsgType::Pong => decode_pong(&frame.payload).ok().map(ShellInbox::Pong),
                    MsgType::CaretRect => decode_caret_rect(&frame.payload)
                        .ok()
                        .map(|(rect, win)| ShellInbox::CaretRect(rect, win)),
                    // Empty-payload wake: a fresh frame is in the IOSurface.
                    // Mapping it to Some(..) is what makes `proxy.wake()`
                    // fire below → user_event → present.
                    MsgType::FrameRendered => Some(ShellInbox::FrameRendered),
                    MsgType::PaneBadgeClicked => {
                        marspot::shell_proto::decode_pane_badge_clicked(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneBadgeClicked)
                    }
                    MsgType::PaneFocused => {
                        marspot::shell_proto::decode_pane_focused(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneFocused)
                    }
                    MsgType::PaneBadgeMenuRequest => {
                        marspot::shell_proto::decode_pane_badge_menu_request(&frame.payload)
                            .ok()
                            .map(|(sid, x, y)| ShellInbox::PaneBadgeMenuRequest(sid, x, y))
                    }
                    MsgType::PaneBadgeMenuAction => {
                        marspot::shell_proto::decode_pane_badge_menu_action(&frame.payload)
                            .ok()
                            .map(|(sid, tag)| ShellInbox::PaneBadgeMenuAction(sid, tag))
                    }
                    MsgType::PaneSessionKey => {
                        marspot::shell_proto::decode_pane_session_key(&frame.payload)
                            .ok()
                            .map(|(sid, ev)| ShellInbox::PaneSessionKey(sid, ev))
                    }
                    MsgType::PaneSessionUserEscape => {
                        marspot::shell_proto::decode_pane_session_user_escape(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneSessionUserEscape)
                    }
                    MsgType::WindowOpenRequest => {
                        marspot::shell_proto::decode_window_open_request_at(&frame.payload)
                            .ok()
                            .map(|(idx, hint)| ShellInbox::WindowOpenRequest(idx, hint))
                    }
                    MsgType::WindowCloseRequest => {
                        marspot::shell_proto::decode_window_close_request(&frame.payload)
                            .ok()
                            .map(ShellInbox::WindowCloseRequest)
                    }
                    MsgType::IconMaskWanted => marspot::shell_proto::decode_icon_mask_wanted(&frame.payload)
                        .ok()
                        .map(|(symbol, point_px)| ShellInbox::IconMaskWanted(symbol, point_px)),
                    // kept where the supervisor reads it, not queued:
                    // a pane that prints reports every second
                    MsgType::PaneAwake => {
                        if let Ok(a) = marspot::shell_proto::decode_pane_awake(&frame.payload) {
                            pane_wakes::record(&a);
                            crate::pump::knock();
                        }
                        None
                    }
                    // Unknown frames are ignored — keeps forward
                    // compatibility while the protocol grows.
                    _ => None,
                };
                if let Some(m) = msg {
                    if tx.send(m).is_err() {
                        return;
                    }
                    proxy.wake();
                }
            }
            Err(e) => {
                lx_error!("shell.control_reader.error", &format!("{e}"));
                return proxy.wake();
            }
        }
    }
}
